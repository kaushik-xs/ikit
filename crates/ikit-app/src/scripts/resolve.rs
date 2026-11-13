//! Resolving a script's params from ikit's state: workloads, targets, service instances and
//! their env (with keychain secrets).

use std::collections::BTreeMap;

use ikit_core::script::{Field, MASK, ServiceField, Side, TargetField};
use ikit_core::{
    Connection, CsvInput, HookPhase, ScriptParam, ScriptSpec, ServiceInstance, SourcePath, Target,
    Workload,
};
use serde::Serialize;

use crate::deploy::Place;
use crate::targets::ssh_args;
use crate::{App, AppError, Result};

/// One workload a script reads.
pub(crate) struct WlCtx {
    pub wl: Workload,
    pub target: Target,
    /// The compose project folder on the target; `None` for targets ikit cannot deploy to.
    pub dir: Option<String>,
    pub services: Vec<ServiceInstance>,
}

impl WlCtx {
    fn service(&self, name: &str) -> Result<&ServiceInstance> {
        self.services
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| AppError::NotFound(format!("service `{name}` in {}", self.wl.name)))
    }

    fn not_ssh(&self) -> AppError {
        AppError::Invalid(format!(
            "target `{}` of {} is not reached over ssh",
            self.target.name, self.wl.name
        ))
    }

    /// The ssh options before the destination, space-separated.
    fn ssh_options(&self) -> Option<String> {
        let dest = self.target.connection.ssh_destination()?;
        let args = ssh_args(&self.target.connection);
        Some(
            args.iter()
                .filter(|a| **a != dest)
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
}

/// Everything a script's params can read.
pub(crate) struct Ctx {
    pub org: String,
    pub primary: WlCtx,
    pub from: Option<WlCtx>,
    pub phase: Option<HookPhase>,
    pub run_id: Option<i64>,
}

/// A param as it will be set, for previews and history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedParam {
    pub name: String,
    /// The source path, `value`, or `override`.
    pub source: String,
    /// Masked when secret; empty when unset.
    pub value: String,
    pub secret: bool,
    /// `default` when the default was used, `unset` when it has no value and is optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The CSV a script takes, built from the services' env (or given as a file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InputPreview {
    /// `services` or `file`.
    pub source: String,
    /// The file's lines, with secret values masked.
    pub lines: Vec<String>,
    /// Services without a value for every column.
    pub skipped: Vec<String>,
}

/// A param's value: fixed, or read from ikit's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParamValue {
    Value(String),
    Source(SourcePath),
}

/// `@source` reads ikit's state; anything else is the value itself.
pub(crate) fn parse_param_value(spec: &str) -> std::result::Result<ParamValue, String> {
    match spec.strip_prefix('@') {
        Some(src) => Ok(ParamValue::Source(src.parse()?)),
        None => Ok(ParamValue::Value(spec.to_owned())),
    }
}

/// Parses `NAME=VALUE` / `NAME=@source` overrides.
pub(crate) fn parse_overrides(
    raw: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, ParamValue>> {
    raw.iter()
        .map(|(name, spec)| {
            check_param_name(name)?;
            let v = parse_param_value(spec)
                .map_err(|e| AppError::Invalid(format!("param {name}: {e}")))?;
            Ok((name.clone(), v))
        })
        .collect()
}

pub(crate) fn check_param_name(name: &str) -> Result<()> {
    if !ikit_core::is_env_name(name) {
        return Err(AppError::Invalid(format!(
            "invalid param name `{name}`: use letters, digits and `_`, not starting with a digit"
        )));
    }
    if name.starts_with("IKIT_") {
        return Err(AppError::Invalid(format!(
            "param `{name}`: names starting with IKIT_ are set by ikit"
        )));
    }
    Ok(())
}

/// Service env read so far, per side and service.
type EnvCache = BTreeMap<(bool, String), BTreeMap<String, String>>;

impl App {
    pub(crate) async fn wl_ctx(&self, wl: Workload) -> Result<WlCtx> {
        let target = self.target_show(&wl.target).await?;
        let dir = match self.place(&target, &wl.name) {
            Ok(Place::Local(d)) => Some(d.display().to_string()),
            Ok(Place::Ssh { dir, .. }) => Some(dir),
            Err(_) => None,
        };
        let services = self.services_list(&wl.name).await?;
        Ok(WlCtx {
            wl,
            target,
            dir,
            services,
        })
    }

    /// The value of `path` and whether it is secret. `Ok(None)` is a missing value (an
    /// unset env var, an unpublished port); structural problems are errors.
    async fn resolve_source(
        &self,
        ctx: &Ctx,
        cache: &mut EnvCache,
        path: &SourcePath,
    ) -> Result<Option<(String, bool)>> {
        let side = match path.side {
            Side::Primary => &ctx.primary,
            Side::From => ctx.from.as_ref().ok_or_else(|| {
                AppError::Invalid(format!(
                    "`{path}` reads the --from workload; pass --from <workload>"
                ))
            })?,
        };
        let plain = |s: String| Ok(Some((s, false)));
        match &path.field {
            Field::Org => plain(ctx.org.clone()),
            Field::Workload => plain(side.wl.name.clone()),
            Field::Env => plain(side.wl.env.clone()),
            Field::Phase => Ok(ctx.phase.map(|p| (p.to_string(), false))),
            Field::RunId => Ok(ctx.run_id.map(|r| (r.to_string(), false))),
            Field::Target(t) => {
                let conn = &side.target.connection;
                match (t, conn) {
                    (TargetField::Name, _) => plain(side.target.name.clone()),
                    (TargetField::Dir, _) => Ok(side.dir.clone().map(|d| (d, false))),
                    (TargetField::Host, Connection::Ssh { host, .. }) => plain(host.clone()),
                    (TargetField::Host, Connection::Local) => plain("127.0.0.1".to_owned()),
                    (TargetField::User, Connection::Ssh { user, .. }) => {
                        Ok(user.clone().map(|u| (u, false)))
                    }
                    (TargetField::Port, Connection::Ssh { port, .. }) => {
                        plain(port.unwrap_or(22).to_string())
                    }
                    (TargetField::Ssh, Connection::Ssh { .. }) => {
                        plain(conn.ssh_destination().unwrap_or_default())
                    }
                    (TargetField::SshArgs, Connection::Ssh { .. }) => {
                        plain(side.ssh_options().unwrap_or_default())
                    }
                    (TargetField::User, Connection::Local) => Ok(None),
                    _ => Err(side.not_ssh()),
                }
            }
            Field::Service { name, field } => {
                let svc = side.service(name)?;
                match field {
                    ServiceField::Container => plain(format!("{}-{}-1", side.wl.name, svc.name)),
                    ServiceField::Host => plain(svc.name.clone()),
                    ServiceField::Image => plain(svc.image.clone()),
                    ServiceField::Version => plain(svc.version.to_string()),
                    ServiceField::Port(p) => Ok(svc
                        .publish
                        .iter()
                        .find(|x| x.container == *p && !x.udp)
                        .map(|x| (x.host.to_string(), false))),
                    ServiceField::Env(var) => {
                        let key = (path.side == Side::From, svc.name.clone());
                        if !cache.contains_key(&key) {
                            let env = self.service_env(&side.wl.name, &svc.name).await?;
                            cache.insert(key.clone(), env);
                        }
                        let secret = svc.secret_env.contains(var);
                        Ok(cache[&key].get(var).map(|v| (v.clone(), secret)))
                    }
                }
            }
        }
    }

    /// The params' values in declaration order (overrides last), as env pairs, and their
    /// preview.
    pub(crate) async fn resolve_params(
        &self,
        ctx: &Ctx,
        spec: &ScriptSpec,
        overrides: &BTreeMap<String, ParamValue>,
    ) -> Result<(Vec<(String, String)>, Vec<ResolvedParam>)> {
        let mut cache = EnvCache::new();
        let mut env = Vec::new();
        let mut shown = Vec::new();
        let declared: BTreeMap<&str, &ScriptParam> =
            spec.params.iter().map(|p| (p.name.as_str(), p)).collect();
        let mut names: Vec<&str> = spec.params.iter().map(|p| p.name.as_str()).collect();
        names.extend(
            overrides
                .keys()
                .map(String::as_str)
                .filter(|n| !declared.contains_key(n)),
        );
        for name in names {
            let param = declared.get(name);
            let forced_secret = param.is_some_and(|p| p.secret);
            let (source, found, required, default) = match overrides.get(name) {
                Some(ParamValue::Value(v)) => {
                    ("override".to_owned(), Some((v.clone(), false)), true, None)
                }
                Some(ParamValue::Source(src)) => (
                    format!("override @{src}"),
                    self.resolve_source(ctx, &mut cache, src).await?,
                    true,
                    None,
                ),
                None => {
                    let p = param.expect("declared");
                    let found = match (&p.from, &p.value) {
                        (Some(src), _) => self.resolve_source(ctx, &mut cache, src).await?,
                        (None, Some(v)) => Some((v.clone(), false)),
                        (None, None) => None,
                    };
                    (p.describe(), found, p.required, p.default.clone())
                }
            };
            let (value, secret, note) = match (found, default) {
                (Some((v, s)), _) => (Some(v), s || forced_secret, None),
                (None, Some(d)) => (Some(d), forced_secret, Some("default".to_owned())),
                (None, None) if required => {
                    return Err(AppError::Invalid(format!(
                        "param {name}: `{source}` has no value in {} (set it, give the param a default, or pass --param {name}=...)",
                        ctx.primary.wl.name
                    )));
                }
                (None, None) => (None, forced_secret, Some("unset".to_owned())),
            };
            if let Some(v) = &value {
                env.push((name.to_owned(), v.clone()));
            }
            shown.push(ResolvedParam {
                name: name.to_owned(),
                source,
                value: match &value {
                    Some(_) if secret => MASK.to_owned(),
                    Some(v) => v.clone(),
                    None => String::new(),
                },
                secret,
                note,
            });
        }
        Ok((env, shown))
    }

    /// One CSV row per service of the primary workload with a value for every column.
    /// Returns the file and its preview (secret values masked).
    pub(crate) async fn render_csv(
        &self,
        ctx: &Ctx,
        csv: &CsvInput,
    ) -> Result<(String, InputPreview)> {
        let mut content = format!("{}\n", csv.header);
        let mut lines = vec![csv.header.clone()];
        let mut skipped = Vec::new();
        let mut services: Vec<&ServiceInstance> = ctx
            .primary
            .services
            .iter()
            .filter(|s| csv.services.is_none_or(|g| s.group == g))
            .collect();
        services.sort_by(|a, b| a.name.cmp(&b.name));
        for svc in services {
            let env = self.service_env(&ctx.primary.wl.name, &svc.name).await?;
            let row: Option<Vec<(&String, bool)>> = csv
                .columns
                .iter()
                .map(|c| {
                    env.get(c)
                        .filter(|v| !v.is_empty())
                        .map(|v| (v, svc.secret_env.contains(c)))
                })
                .collect();
            let Some(row) = row else {
                skipped.push(svc.name.clone());
                continue;
            };
            if let Some((col, _)) = csv
                .columns
                .iter()
                .zip(&row)
                .find(|(_, (v, _))| v.contains([',', '"', '\n', '\r']))
            {
                return Err(AppError::Invalid(format!(
                    "{col} of `{}` contains a comma, quote or newline, which the CSV cannot hold",
                    svc.name
                )));
            }
            content.push_str(
                &row.iter()
                    .map(|(v, _)| v.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            );
            content.push('\n');
            lines.push(
                row.iter()
                    .map(|(v, s)| if *s { MASK } else { v.as_str() })
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        Ok((
            content,
            InputPreview {
                source: "services".to_owned(),
                lines,
                skipped,
            },
        ))
    }
}

/// The env every script gets, before its params.
pub(crate) fn base_env(ctx: &Ctx, script: &str, checksum: &str) -> Vec<(String, String)> {
    let p = &ctx.primary;
    let mut env = vec![
        ("IKIT_ORG".to_owned(), ctx.org.clone()),
        ("IKIT_WORKLOAD".to_owned(), p.wl.name.clone()),
        ("IKIT_ENV".to_owned(), p.wl.env.clone()),
        ("IKIT_TARGET".to_owned(), p.target.name.clone()),
        (
            "IKIT_PHASE".to_owned(),
            ctx.phase.map(|p| p.to_string()).unwrap_or_default(),
        ),
        (
            "IKIT_RUN_ID".to_owned(),
            ctx.run_id.map(|r| r.to_string()).unwrap_or_default(),
        ),
        ("IKIT_SCRIPT".to_owned(), script.to_owned()),
        ("IKIT_SCRIPT_CHECKSUM".to_owned(), checksum.to_owned()),
    ];
    if let Some(from) = &ctx.from {
        for (prefix, side) in [("IKIT_FROM", from), ("IKIT_TO", p)] {
            env.push((format!("{prefix}_WORKLOAD"), side.wl.name.clone()));
            if let Some(dest) = side.target.connection.ssh_destination() {
                env.push((format!("{prefix}_SSH"), dest));
                env.push((
                    format!("{prefix}_SSH_ARGS"),
                    side.ssh_options().unwrap_or_default(),
                ));
            }
            if let Some(dir) = &side.dir {
                env.push((format!("{prefix}_DIR"), dir.clone()));
            }
        }
    }
    env
}
