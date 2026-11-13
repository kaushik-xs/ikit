//! The org's script library: shell (or Python) scripts imported once and run against any
//! workload, with their env resolved from ikit's state. See `docs/scripts.md`.

mod engine;
mod front_matter;
mod resolve;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use ikit_core::script::DEFAULT_TIMEOUT_SECS;
use ikit_core::{
    Hook, HookAction, HookPhase, Interpreter, RunAction, RunResult, RunsOn, Script, ScriptAction,
    ScriptParam, ScriptRunRecord, ScriptSpec, Workload, WorkloadStatus,
};
use ikit_state::{NewRun, NewScript, ScriptExecution};
use serde::Serialize;

use crate::deploy::{Place, sh};
use crate::exec::ExecOutput;
use crate::hooks_sql::checksum;
use crate::{App, AppError, Result};
use engine::{Payload, WorkDir};
use resolve::{Ctx, ParamValue, base_env, check_param_name, parse_overrides};
pub use resolve::{InputPreview, ResolvedParam};

/// The longest a script may be allowed to run.
const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;
/// How much of a script's output is kept in its history.
const OUTPUT_TAIL: usize = 4000;

/// `ikit script add`: a file and flags that fill in or override its `# ikit:` header.
#[derive(Debug, Clone, Default)]
pub struct ScriptAddRequest {
    pub file: PathBuf,
    pub name: Option<String>,
    pub runs_on: Option<String>,
    pub interpreter: Option<Interpreter>,
    pub description: Option<String>,
    /// `NAME -> value`, or `NAME -> @source`; added to or replacing the header's params.
    pub params: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportStatus {
    Added,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScriptAddReport {
    pub status: ImportStatus,
    #[serde(flatten)]
    pub script: Script,
}

/// `ikit script run`.
#[derive(Debug, Clone, Default)]
pub struct ScriptRunRequest {
    pub script: String,
    /// The workload it runs against (`--to`, or the selected one).
    pub workload: String,
    /// A second workload its `from.` params read.
    pub from: Option<String>,
    /// `NAME -> value`, or `NAME -> @source`.
    pub params: BTreeMap<String, String>,
    /// CSV content replacing the rows built from the services.
    pub csv: Option<String>,
    /// Appended after the script's own args.
    pub args: Vec<String>,
    pub timeout: Option<Duration>,
    /// Refuse to run if the script changed since this checksum was previewed.
    pub expected_checksum: Option<String>,
}

/// What a run would do, with secrets masked.
#[derive(Debug, Clone, Serialize)]
pub struct ScriptPreview {
    pub script: String,
    pub checksum: String,
    pub runs_on: RunsOn,
    pub workload: String,
    pub env: String,
    /// Where it runs, e.g. `ssh ubuntu@10.0.0.5, in .ikit/acme/wl-acme-dev-001`.
    pub location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub params: Vec<ResolvedParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<InputPreview>,
    pub command: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScriptReport {
    pub run_id: i64,
    pub script: String,
    pub checksum: String,
    pub workload: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Everything resolved for one execution.
struct Prepared {
    preview: ScriptPreview,
    env: Vec<(String, String)>,
    input: Option<String>,
    input_arg: bool,
    args: Vec<String>,
    timeout: Duration,
}

fn invalid(e: impl std::fmt::Display) -> AppError {
    AppError::Invalid(e.to_string())
}

/// The end of a script's output, for its history.
fn tail(out: &ExecOutput) -> String {
    let all = format!("{}{}", out.stdout, out.stderr);
    let chars: Vec<char> = all.chars().collect();
    chars[chars.len().saturating_sub(OUTPUT_TAIL)..]
        .iter()
        .collect()
}

fn masked_params(params: &[ResolvedParam]) -> BTreeMap<String, String> {
    params
        .iter()
        .map(|p| (p.name.clone(), p.value.clone()))
        .collect()
}

/// Validates what a script declares; shared by import and by flags.
fn validate_spec(spec: &ScriptSpec) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for p in &spec.params {
        check_param_name(&p.name)?;
        if !seen.insert(&p.name) {
            return Err(invalid(format!("param {} is declared twice", p.name)));
        }
    }
    if let Some(csv) = &spec.csv {
        if csv.columns.is_empty() {
            return Err(invalid("input.csv needs at least one column"));
        }
        if let Some(bad) = csv.columns.iter().find(|c| !ikit_core::is_env_name(c)) {
            return Err(invalid(format!("invalid input.csv column `{bad}`")));
        }
        if csv.header.contains(['\n', '\r']) {
            return Err(invalid("input.csv header must be one line"));
        }
    }
    if let Some(bad) = spec.args.iter().find(|a| a.contains('\0')) {
        return Err(invalid(format!("invalid argument {bad:?}")));
    }
    if spec.timeout_secs == 0 || spec.timeout_secs > MAX_TIMEOUT_SECS {
        return Err(invalid(format!(
            "timeout must be between 1 and {MAX_TIMEOUT_SECS} seconds"
        )));
    }
    Ok(())
}

impl App {
    /// Imports a script file (or updates the script of the same name).
    pub async fn script_add(&self, req: ScriptAddRequest) -> Result<ScriptAddReport> {
        let body = std::fs::read_to_string(&req.file)
            .map_err(|e| AppError::Invalid(format!("cannot read {}: {e}", req.file.display())))?;
        if body.trim().is_empty() {
            return Err(invalid(format!("{} is empty", req.file.display())));
        }
        if body.contains('\r') {
            return Err(invalid(format!(
                "{} has Windows (CRLF) line endings; convert it to LF first (e.g. `dos2unix`)",
                req.file.display()
            )));
        }
        if body.contains('\0') {
            return Err(invalid(format!(
                "{} is not a text file",
                req.file.display()
            )));
        }
        let header = front_matter::parse(&body)
            .map_err(|e| invalid(format!("{}: {e}", req.file.display())))?
            .unwrap_or_default();

        let name = req.name.or(header.name).ok_or_else(|| {
            invalid("the script has no name: add `name:` to its `# ikit:` header or pass --name")
        })?;
        if !ikit_core::is_slug(&name) {
            return Err(invalid(format!(
                "invalid script name `{name}`: use {}",
                ikit_core::SLUG_RULE
            )));
        }
        let runs_on: RunsOn = req
            .runs_on
            .or(header.runs_on)
            .ok_or_else(|| invalid("say where the script runs: add `runs_on: target|local|service:<name>` to its header or pass --runs-on"))?
            .parse()
            .map_err(invalid)?;
        let interpreter = req
            .interpreter
            .or(header.interpreter)
            .unwrap_or_else(|| Interpreter::from_shebang(&body));
        let mut params = header.params;
        for (pname, spec) in parse_overrides(&req.params)? {
            let (from, value) = match spec {
                ParamValue::Source(s) => (Some(s), None),
                ParamValue::Value(v) => (None, Some(v)),
            };
            let param = ScriptParam {
                name: pname.clone(),
                from,
                value,
                default: None,
                required: true,
                secret: false,
            };
            match params.iter_mut().find(|p| p.name == pname) {
                Some(p) => *p = param,
                None => params.push(param),
            }
        }
        let spec = ScriptSpec {
            params,
            csv: header.csv,
            args: header.args,
            timeout_secs: header.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS),
        };
        validate_spec(&spec)?;
        let description = req.description.or(header.description).unwrap_or_default();
        let sum = checksum(&body);
        let source = std::fs::canonicalize(&req.file)
            .unwrap_or(req.file.clone())
            .display()
            .to_string();

        let existing = self.store.scripts().get(&name).await?;
        if let Some(old) = &existing
            && old.checksum == sum
            && old.spec == spec
            && old.runs_on == runs_on
            && old.interpreter == interpreter
            && old.description == description
        {
            return Ok(ScriptAddReport {
                status: ImportStatus::Unchanged,
                script: old.clone(),
            });
        }
        let script = self
            .store
            .scripts()
            .upsert(NewScript {
                name: &name,
                description: &description,
                interpreter,
                runs_on: &runs_on,
                body: &body,
                checksum: &sum,
                spec: &spec,
                source: Some(&source),
            })
            .await?;
        Ok(ScriptAddReport {
            status: if existing.is_some() {
                ImportStatus::Updated
            } else {
                ImportStatus::Added
            },
            script,
        })
    }

    pub async fn script_list(&self) -> Result<Vec<Script>> {
        Ok(self.store.scripts().list().await?)
    }

    pub async fn script_show(&self, name: &str) -> Result<Script> {
        self.store
            .scripts()
            .get(name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("script `{name}` (see `ikit script list`)")))
    }

    /// Hooks that run the script, as `workload/hook`.
    async fn script_users(&self, name: &str) -> Result<Vec<String>> {
        let mut users = Vec::new();
        for wl in self.store.workloads().list(None).await? {
            for hook in self.store.hooks().list(wl.id).await? {
                if matches!(&hook.action, HookAction::Script(a) if a.script == name) {
                    users.push(format!("hook `{}` in {}", hook.name, wl.name));
                }
            }
        }
        Ok(users)
    }

    /// Removes the script and its history; refused while a hook runs it.
    pub async fn script_remove(&self, name: &str) -> Result<()> {
        self.script_show(name).await?;
        let users = self.script_users(name).await?;
        if !users.is_empty() {
            return Err(AppError::InUse {
                what: format!("script `{name}`"),
                by: users,
            });
        }
        self.store.scripts().delete(name).await?;
        Ok(())
    }

    pub async fn script_history(
        &self,
        script: Option<&str>,
        workload: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ScriptRunRecord>> {
        Ok(self
            .store
            .scripts()
            .history(script, workload, limit)
            .await?)
    }

    /// Checks that a hook's script exists and its overrides parse.
    pub(crate) async fn script_hook_check(&self, action: &ScriptAction) -> Result<()> {
        self.script_show(&action.script).await?;
        parse_overrides(&action.params)?;
        Ok(())
    }

    async fn script_ctx(
        &self,
        wl: Workload,
        from: Option<&str>,
        phase: Option<HookPhase>,
        run_id: Option<i64>,
    ) -> Result<Ctx> {
        let from = match from {
            Some(f) if f == wl.name => {
                return Err(invalid(
                    "--from and the workload it runs against are the same",
                ));
            }
            Some(f) => Some(self.wl_ctx(self.workload_get(f).await?).await?),
            None => None,
        };
        Ok(Ctx {
            org: self.org.name.clone(),
            primary: self.wl_ctx(wl).await?,
            from,
            phase,
            run_id,
        })
    }

    async fn prepare(
        &self,
        script: &Script,
        ctx: &Ctx,
        overrides: &BTreeMap<String, ParamValue>,
        csv: Option<&str>,
        extra_args: &[String],
        timeout: Option<Duration>,
    ) -> Result<Prepared> {
        if let RunsOn::Service(svc) = &script.runs_on
            && !ctx.primary.services.iter().any(|s| &s.name == svc)
        {
            return Err(AppError::NotFound(format!(
                "service `{svc}` in {} (script `{}` runs in it)",
                ctx.primary.wl.name, script.name
            )));
        }
        let (params_env, params) = self.resolve_params(ctx, &script.spec, overrides).await?;
        let (input, input_preview) = match (csv, &script.spec.csv) {
            (Some(content), _) => {
                let lines = content.lines().map(str::to_owned).collect();
                (
                    Some(content.to_owned()),
                    Some(InputPreview {
                        source: "file".to_owned(),
                        lines,
                        skipped: vec![],
                    }),
                )
            }
            (None, Some(spec)) => {
                let (content, preview) = self.render_csv(ctx, spec).await?;
                (Some(content), Some(preview))
            }
            (None, None) => (None, None),
        };
        let input_arg = script.spec.csv.as_ref().is_none_or(|c| c.arg);
        let mut args = script.spec.args.clone();
        args.extend(extra_args.iter().cloned());
        if let Some(bad) = args.iter().find(|a| a.contains('\0')) {
            return Err(invalid(format!("invalid argument {bad:?}")));
        }
        let timeout_secs = timeout.map_or(script.spec.timeout_secs, |t| t.as_secs().max(1));
        if timeout_secs > MAX_TIMEOUT_SECS {
            return Err(invalid(format!(
                "timeout must be at most {MAX_TIMEOUT_SECS} seconds"
            )));
        }
        let mut env = base_env(ctx, &script.name, &script.checksum);
        env.extend(params_env);

        let file_name = script.file_name();
        let payload = Payload {
            name: &script.name,
            file_name: &file_name,
            interpreter: script.interpreter,
            body: &script.body,
            checksum: &script.checksum,
            input: input.as_deref(),
            input_arg,
            env: &env,
            args: &args,
            workdir: WorkDir::Here,
        };
        let p = &ctx.primary;
        let location = match &script.runs_on {
            RunsOn::Target => format!(
                "{} ({}), in {}",
                p.target.name,
                p.target.connection.describe(),
                p.dir.as_deref().unwrap_or("?")
            ),
            RunsOn::Service(s) => format!("service `{s}` of {}, in a temp folder", p.wl.name),
            RunsOn::Local => "this machine, in a temp folder".to_owned(),
        };
        Ok(Prepared {
            preview: ScriptPreview {
                script: script.name.clone(),
                checksum: script.checksum.clone(),
                runs_on: script.runs_on.clone(),
                workload: p.wl.name.clone(),
                env: p.wl.env.clone(),
                location,
                from: ctx.from.as_ref().map(|f| f.wl.name.clone()),
                params,
                input: input_preview,
                command: engine::command_line(&payload),
                timeout_secs,
            },
            env,
            input,
            input_arg,
            args,
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    /// Runs the prepared script where it belongs. A failing script is an `Ok` output with a
    /// non-zero code; errors are about reaching the place it runs.
    async fn execute(&self, script: &Script, ctx: &Ctx, prepared: &Prepared) -> Result<ExecOutput> {
        let wl = &ctx.primary.wl;
        let workdir = match script.runs_on {
            RunsOn::Target => WorkDir::Here,
            RunsOn::Service(_) | RunsOn::Local => WorkDir::Temp,
        };
        let file_name = script.file_name();
        let program = engine::bootstrap(&Payload {
            name: &script.name,
            file_name: &file_name,
            interpreter: script.interpreter,
            body: &script.body,
            checksum: &script.checksum,
            input: prepared.input.as_deref(),
            input_arg: prepared.input_arg,
            env: &prepared.env,
            args: &prepared.args,
            workdir,
        });
        let stdin = Some(program.as_bytes());
        let timeout = prepared.timeout;
        match &script.runs_on {
            RunsOn::Local => Ok(self
                .exec
                .run("sh", &["-s".to_owned()], stdin, timeout)
                .await?),
            RunsOn::Target => match self.place(&ctx.primary.target, &wl.name)? {
                Place::Local(dir) => {
                    std::fs::create_dir_all(&dir)?;
                    let cmd = format!("cd {} && exec sh -s", sh(&dir.display().to_string()));
                    Ok(self
                        .exec
                        .run("sh", &["-c".to_owned(), cmd], stdin, timeout)
                        .await?)
                }
                Place::Ssh { args, dir } => {
                    let cmd = format!("mkdir -p {d} && cd {d} && sh -s", d = sh(&dir));
                    self.ssh(&args, &cmd, stdin, timeout).await
                }
            },
            RunsOn::Service(svc) => {
                if wl.status != WorkloadStatus::Applied {
                    return Err(invalid(format!(
                        "{} is {}; script `{}` runs in service `{svc}`, which must be deployed (`ikit apply`)",
                        wl.name, wl.status, script.name
                    )));
                }
                let place = self.place(&ctx.primary.target, &wl.name)?;
                let args = ["exec", "-T", svc.as_str(), "sh", "-s"];
                self.compose_with(&place, &wl.name, &args, stdin, timeout)
                    .await
            }
        }
    }

    /// What `script_run` would do, without running anything.
    pub async fn script_preview(&self, req: &ScriptRunRequest) -> Result<ScriptPreview> {
        let script = self.script_show(&req.script).await?;
        let wl = self.workload_get(&req.workload).await?;
        if script.spec.uses_from() && req.from.is_none() {
            return Err(invalid(format!(
                "script `{}` reads a second workload; pass --from <workload>",
                script.name
            )));
        }
        let ctx = self.script_ctx(wl, req.from.as_deref(), None, None).await?;
        let overrides = parse_overrides(&req.params)?;
        Ok(self
            .prepare(
                &script,
                &ctx,
                &overrides,
                req.csv.as_deref(),
                &req.args,
                req.timeout,
            )
            .await?
            .preview)
    }

    /// Runs a script against a workload, recorded as a run of that workload (which is
    /// locked meanwhile). A script that exits non-zero is reported with `ok: false`.
    pub async fn script_run(&self, req: &ScriptRunRequest, actor: &str) -> Result<ScriptReport> {
        let script = self.script_show(&req.script).await?;
        if let Some(expected) = &req.expected_checksum
            && expected != &script.checksum
        {
            return Err(invalid(format!(
                "script `{}` changed since it was previewed; run it again",
                script.name
            )));
        }
        if script.spec.uses_from() && req.from.is_none() {
            return Err(invalid(format!(
                "script `{}` reads a second workload; pass --from <workload>",
                script.name
            )));
        }
        let overrides = parse_overrides(&req.params)?;
        let wl = self.workload_get(&req.workload).await?;
        if let Some(from) = &req.from {
            self.workload_get(from).await?;
        }
        let holder = format!("{actor} (pid {})", std::process::id());
        self.lock(&wl, &holder).await?;
        let result = async {
            let plan_json = serde_json::json!({
                "script": script.name,
                "checksum": script.checksum,
                "runs_on": script.runs_on,
                "from": req.from,
            })
            .to_string();
            let run = NewRun {
                action: RunAction::Script,
                actor,
                plan_json: &plan_json,
                compose_yaml: None,
            };
            let run_id = self.store.workloads().start_run(wl.id, run).await?;
            let outcome = async {
                let ctx = self
                    .script_ctx(wl.clone(), req.from.as_deref(), None, Some(run_id))
                    .await?;
                let prepared = self
                    .prepare(
                        &script,
                        &ctx,
                        &overrides,
                        req.csv.as_deref(),
                        &req.args,
                        req.timeout,
                    )
                    .await?;
                let out = self.execute(&script, &ctx, &prepared).await?;
                Ok::<_, AppError>((prepared, out))
            }
            .await;
            let (prepared, out) = match outcome {
                Ok(v) => v,
                Err(e) => {
                    self.store
                        .workloads()
                        .finish_run(run_id, RunResult::Failed, Some(&e.to_string()))
                        .await?;
                    return Err(e);
                }
            };
            let ok = out.success();
            let error = (!ok).then(|| out.failure_detail());
            let params = masked_params(&prepared.preview.params);
            let output_tail = tail(&out);
            self.store
                .scripts()
                .record(
                    script.id,
                    ScriptExecution {
                        run_id: Some(run_id),
                        workload: &wl.name,
                        from_workload: req.from.as_deref(),
                        hook: None,
                        checksum: &script.checksum,
                        params: &params,
                        result: if ok { RunResult::Ok } else { RunResult::Failed },
                        exit_code: out.code,
                        error: error.as_deref(),
                        output_tail: Some(&output_tail),
                    },
                )
                .await?;
            let (result, error) = if ok {
                (RunResult::Ok, None)
            } else {
                (RunResult::Failed, error)
            };
            self.store
                .workloads()
                .finish_run(run_id, result, error.as_deref())
                .await?;
            Ok(ScriptReport {
                run_id,
                script: script.name.clone(),
                checksum: script.checksum.clone(),
                workload: wl.name.clone(),
                from: req.from.clone(),
                ok,
                exit_code: out.code,
                timed_out: out.timed_out,
                stdout: out.stdout,
                stderr: out.stderr,
            })
        }
        .await;
        self.store.workloads().unlock(wl.id, &holder).await?;
        result
    }

    /// Whether a script hook must wait for its service (a first deploy's pre-apply).
    pub(crate) async fn script_hook_waits(
        &self,
        action: &ScriptAction,
        wl: &Workload,
        phase: HookPhase,
    ) -> Result<bool> {
        let script = self.store.scripts().get(&action.script).await?;
        Ok(
            matches!(script.map(|s| s.runs_on), Some(RunsOn::Service(_)))
                && wl.status == WorkloadStatus::Draft
                && phase == HookPhase::PreApply,
        )
    }

    /// Runs a script hook inside an apply, destroy or `hook run` (which holds the lock and
    /// the run). Records the execution; the error is the hook step's failure.
    pub(crate) async fn run_script_hook(
        &self,
        wl: &Workload,
        hook: &Hook,
        action: &ScriptAction,
        phase: HookPhase,
        run_id: Option<i64>,
    ) -> std::result::Result<(), String> {
        let script = self
            .store
            .scripts()
            .get(&action.script)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "script `{}` is not in the library (add it with `ikit script add`)",
                    action.script
                )
            })?;
        if script.spec.uses_from() {
            return Err(format!(
                "script `{}` reads a --from workload, which hooks cannot give it",
                script.name
            ));
        }
        let run = async {
            let overrides = parse_overrides(&action.params)?;
            let ctx = self
                .script_ctx(wl.clone(), None, Some(phase), run_id)
                .await?;
            let prepared = self
                .prepare(&script, &ctx, &overrides, None, &[], None)
                .await?;
            let out = self.execute(&script, &ctx, &prepared).await?;
            Ok::<_, AppError>((prepared, out))
        }
        .await;
        match run {
            Err(e) => {
                let e = e.to_string();
                self.record_hook_execution(&script, wl, hook, run_id, &BTreeMap::new(), None, &e)
                    .await?;
                Err(e)
            }
            Ok((prepared, out)) => {
                let params = masked_params(&prepared.preview.params);
                let error = (!out.success()).then(|| out.failure_detail());
                let detail = error.clone().unwrap_or_default();
                self.record_hook_execution(&script, wl, hook, run_id, &params, Some(&out), &detail)
                    .await?;
                error.map_or(Ok(()), Err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_hook_execution(
        &self,
        script: &Script,
        wl: &Workload,
        hook: &Hook,
        run_id: Option<i64>,
        params: &BTreeMap<String, String>,
        out: Option<&ExecOutput>,
        error: &str,
    ) -> std::result::Result<(), String> {
        let failed = !error.is_empty();
        let output_tail = out.map(tail);
        self.store
            .scripts()
            .record(
                script.id,
                ScriptExecution {
                    run_id,
                    workload: &wl.name,
                    from_workload: None,
                    hook: Some(&hook.name),
                    checksum: &script.checksum,
                    params,
                    result: if failed {
                        RunResult::Failed
                    } else {
                        RunResult::Ok
                    },
                    exit_code: out.and_then(|o| o.code),
                    error: failed.then_some(error),
                    output_tail: output_tail.as_deref(),
                },
            )
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ikit_core::registry::{Category, ImportMode, RegistryFile};
    use ikit_core::{Connection, HookKind};

    use super::*;
    use crate::testing::app;
    use crate::{Exec, HookRequest, NewHookRequest, ServiceRequest};

    /// Records every command with its stdin and exits with `code`.
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<(String, Vec<String>, String)>>,
        code: Mutex<i32>,
    }

    impl Recorder {
        fn calls(&self) -> Vec<(String, Vec<String>, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Exec for Recorder {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            stdin: Option<&[u8]>,
            _: Duration,
        ) -> io::Result<ExecOutput> {
            self.calls.lock().unwrap().push((
                program.to_owned(),
                args.to_vec(),
                String::from_utf8_lossy(stdin.unwrap_or_default()).into_owned(),
            ));
            let code = *self.code.lock().unwrap();
            Ok(ExecOutput {
                code: Some(code),
                stdout: "done\n".into(),
                stderr: if code == 0 {
                    String::new()
                } else {
                    "boom\n".into()
                },
                ..Default::default()
            })
        }
    }

    const APPS: &str = r#"
services:
  - name: core
    category: app
    versions:
      - version: 1.0.0
        image: ghcr.io/acme/core:1.0.0
        env:
          - {name: DB_NAME}
          - {name: DB_USER}
          - {name: DB_PASSWORD, secret: true}
  - name: web
    category: app
    versions:
      - version: 1.0.0
        image: ghcr.io/acme/web:1.0.0
"#;

    const CREATE_DB: &str = r#"#!/usr/bin/env bash
# ikit:
#   name: create-db
#   runs_on: target
#   params:
#     PGCONTAINER: service.postgres.container
#     PGSUPERUSER: { from: service.postgres.env.POSTGRES_USER, default: postgres }
#     PGSUPERPASS: service.postgres.env.POSTGRES_PASSWORD
#   input:
#     csv:
#       columns: [DB_NAME, DB_USER, DB_PASSWORD]
#       services: app
#   args: ["-y"]
set -euo pipefail
echo "creating in $PGCONTAINER"
"#;

    fn svc(
        registry: &str,
        constraint: &str,
        group: Category,
        env: &[(&str, &str)],
    ) -> ServiceRequest {
        ServiceRequest {
            name: None,
            registry: registry.into(),
            constraint: constraint.parse().unwrap(),
            group,
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    fn ssh(host: &str) -> Connection {
        Connection::Ssh {
            host: host.into(),
            user: Some("ubuntu".into()),
            port: Some(2222),
            identity_file: None,
            zerotier: None,
        }
    }

    /// An app with a recorder, an ssh target `vm` and `wl-acme-dev-001` on it holding
    /// postgres, core (with database env) and web.
    async fn setup() -> (tempfile::TempDir, App, Arc<Recorder>) {
        let (dir, app, _) = app().await;
        let rec = Arc::new(Recorder::default());
        let app = app.with_exec(rec.clone());
        let file: RegistryFile = serde_yaml_ng::from_str(APPS).unwrap();
        app.registry_import(&file, ImportMode::Merge).await.unwrap();
        app.target_attach("vm", ssh("10.0.0.5")).await.unwrap();
        let wl = app.workload_create("dev", "vm").await.unwrap().name;
        app.services_add(
            &wl,
            vec![
                svc(
                    "postgres",
                    "16",
                    Category::Data,
                    &[("POSTGRES_PASSWORD", "pg-s3cret")],
                ),
                svc(
                    "core",
                    "1",
                    Category::App,
                    &[
                        ("DB_NAME", "core"),
                        ("DB_USER", "core_user"),
                        ("DB_PASSWORD", "core-s3cret"),
                    ],
                ),
                svc("web", "1", Category::App, &[]),
            ],
        )
        .await
        .unwrap();
        (dir, app, rec)
    }

    async fn add(app: &App, dir: &tempfile::TempDir, file: &str, body: &str) -> ScriptAddReport {
        let path = dir.path().join(file);
        std::fs::write(&path, body).unwrap();
        app.script_add(ScriptAddRequest {
            file: path,
            ..Default::default()
        })
        .await
        .unwrap()
    }

    fn run_req(script: &str) -> ScriptRunRequest {
        ScriptRunRequest {
            script: script.into(),
            workload: "wl-acme-dev-001".into(),
            ..Default::default()
        }
    }

    async fn set_applied(app: &App, wl: &str) {
        let wl = app.workload_get(wl).await.unwrap();
        app.store
            .workloads()
            .set_status(wl.id, WorkloadStatus::Applied)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn imports_are_versioned_by_content() {
        let (dir, app, _) = setup().await;
        let first = add(&app, &dir, "create-db.sh", CREATE_DB).await;
        assert_eq!(first.status, ImportStatus::Added);
        assert_eq!(first.script.interpreter, Interpreter::Bash);
        assert_eq!(first.script.runs_on, RunsOn::Target);
        assert_eq!(first.script.body, CREATE_DB);
        let again = add(&app, &dir, "create-db.sh", CREATE_DB).await;
        assert_eq!(again.status, ImportStatus::Unchanged);
        let edited = add(
            &app,
            &dir,
            "create-db.sh",
            &format!("{CREATE_DB}echo more\n"),
        )
        .await;
        assert_eq!(edited.status, ImportStatus::Updated);
        assert_ne!(edited.script.checksum, first.script.checksum);
        assert_eq!(app.script_list().await.unwrap().len(), 1);

        // No header: flags supply what it needs.
        let path = dir.path().join("plain.sh");
        std::fs::write(&path, "echo $GREETING\n").unwrap();
        let e = app
            .script_add(ScriptAddRequest {
                file: path.clone(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("--name"), "{e}");
        let plain = app
            .script_add(ScriptAddRequest {
                file: path.clone(),
                name: Some("hello".into()),
                runs_on: Some("local".into()),
                params: [("GREETING".to_owned(), "hi".to_owned())].into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(plain.script.interpreter, Interpreter::Sh);
        assert_eq!(plain.script.spec.params[0].value.as_deref(), Some("hi"));

        for (params, needle) in [
            (vec![("IKIT_X", "1")], "set by ikit"),
            (vec![("BAD-NAME", "1")], "invalid param name"),
            (vec![("X", "@target.password")], "invalid source"),
        ] {
            let e = app
                .script_add(ScriptAddRequest {
                    file: path.clone(),
                    name: Some("bad".into()),
                    runs_on: Some("local".into()),
                    params: params
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .collect(),
                    ..Default::default()
                })
                .await
                .unwrap_err();
            assert!(e.to_string().contains(needle), "{e}");
        }
        std::fs::write(&path, "echo hi\r\n").unwrap();
        let e = app
            .script_add(ScriptAddRequest {
                file: path,
                name: Some("crlf".into()),
                runs_on: Some("local".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("CRLF"), "{e}");
    }

    #[tokio::test]
    async fn target_runs_get_secrets_on_stdin_only() {
        let (dir, app, rec) = setup().await;
        let script = add(&app, &dir, "create-db.sh", CREATE_DB).await.script;

        let preview = app.script_preview(&run_req("create-db")).await.unwrap();
        let shown: BTreeMap<&str, &str> = preview
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.value.as_str()))
            .collect();
        assert_eq!(shown["PGCONTAINER"], "wl-acme-dev-001-postgres-1");
        assert_eq!(shown["PGSUPERUSER"], "postgres");
        assert_eq!(shown["PGSUPERPASS"], "<secret>");
        let input = preview.input.as_ref().unwrap();
        assert_eq!(
            input.lines,
            ["db_name,db_user,db_password", "core,core_user,<secret>"]
        );
        assert_eq!(input.skipped, ["web"]);
        assert_eq!(
            preview.command,
            "bash scripts/create-db.sh -y scripts/create-db.input"
        );
        assert!(rec.calls().is_empty(), "previews run nothing");

        let report = app.script_run(&run_req("create-db"), "test").await.unwrap();
        assert!(report.ok && report.stdout == "done\n", "{report:?}");
        let calls = rec.calls();
        assert_eq!(calls.len(), 1);
        let (program, args, stdin) = &calls[0];
        assert_eq!(program, "ssh");
        assert!(args.windows(2).any(|w| w == ["-p", "2222"]), "{args:?}");
        assert!(args.contains(&"ubuntu@10.0.0.5".to_owned()), "{args:?}");
        assert_eq!(
            args.last().unwrap(),
            "mkdir -p '.ikit/acme/wl-acme-dev-001' && cd '.ikit/acme/wl-acme-dev-001' && sh -s"
        );
        for secret in ["pg-s3cret", "core-s3cret"] {
            assert!(
                !args.iter().any(|a| a.contains(secret)),
                "{secret} on the command line: {args:?}"
            );
            assert!(stdin.contains(secret), "{secret} not on stdin");
        }
        assert!(
            stdin.contains("export PGSUPERPASS='pg-s3cret'\n"),
            "{stdin}"
        );
        assert!(stdin.contains("core,core_user,core-s3cret\n"), "{stdin}");
        assert!(
            stdin.contains("bash 'scripts/create-db.sh' -y \"$IKIT_INPUT\" </dev/null"),
            "{stdin}"
        );
        assert!(stdin.contains(&format!("export IKIT_RUN_ID='{}'", report.run_id)));

        let history = app
            .script_history(Some("create-db"), None, 10)
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].params["PGSUPERPASS"], "<secret>");
        assert_eq!(history[0].run_id, Some(report.run_id));
        assert_eq!(history[0].checksum, script.checksum);
        let runs = app.workload_runs("wl-acme-dev-001", 5).await.unwrap();
        assert_eq!(
            (runs[0].action, runs[0].result),
            (RunAction::Script, RunResult::Ok)
        );

        // Overrides: a value, and a source read from the state.
        let mut req = run_req("create-db");
        req.params = [
            ("PGSUPERUSER".to_owned(), "admin".to_owned()),
            (
                "DBNAME".to_owned(),
                "@service.postgres.env.POSTGRES_DB".to_owned(),
            ),
        ]
        .into();
        req.csv = Some("db_name,db_user,db_password\nextra,extra,pw\n".into());
        let preview = app.script_preview(&req).await.unwrap();
        let shown: BTreeMap<&str, &str> = preview
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.value.as_str()))
            .collect();
        assert_eq!((shown["PGSUPERUSER"], shown["DBNAME"]), ("admin", "app"));
        assert_eq!(preview.input.unwrap().source, "file");

        // A script that changed since its preview is refused.
        req.expected_checksum = Some("stale".into());
        let e = app.script_run(&req, "test").await.unwrap_err();
        assert!(e.to_string().contains("changed since"), "{e}");

        // A failing script is reported, and recorded as failed.
        *rec.code.lock().unwrap() = 3;
        let report = app.script_run(&run_req("create-db"), "test").await.unwrap();
        assert!(!report.ok && report.exit_code == Some(3));
        let last = &app.script_history(None, None, 1).await.unwrap()[0];
        assert_eq!(last.result, RunResult::Failed);
        assert!(last.output_tail.as_deref().unwrap().contains("boom"));
        let runs = app.workload_runs("wl-acme-dev-001", 1).await.unwrap();
        assert_eq!(runs[0].result, RunResult::Failed);
    }

    #[tokio::test]
    async fn missing_values_fail_unless_optional() {
        let (dir, app, _) = setup().await;
        add(
            &app,
            &dir,
            "x.sh",
            "# ikit:\n#   name: x\n#   runs_on: target\n#   params:\n#     PORT: service.postgres.port.5432\n#     USER: { from: service.web.env.NOPE, required: false }\n",
        )
        .await;
        let e = app.script_preview(&run_req("x")).await.unwrap_err();
        assert!(
            e.to_string().contains("PORT") && e.to_string().contains("has no value"),
            "{e}"
        );
        let mut req = run_req("x");
        req.params = [("PORT".to_owned(), "5433".to_owned())].into();
        let preview = app.script_preview(&req).await.unwrap();
        assert_eq!(preview.params[1].note.as_deref(), Some("unset"));

        add(
            &app,
            &dir,
            "y.sh",
            "# ikit:\n#   name: y\n#   runs_on: target\n#   params:\n#     H: service.nope.host\n",
        )
        .await;
        let e = app.script_preview(&run_req("y")).await.unwrap_err();
        assert!(e.to_string().contains("service `nope`"), "{e}");
    }

    #[tokio::test]
    async fn local_runs_read_two_workloads() {
        let (dir, app, rec) = setup().await;
        app.target_attach("prod-vm", ssh("10.0.0.9")).await.unwrap();
        let prod = app.workload_create("prod", "prod-vm").await.unwrap().name;
        add(
            &app,
            &dir,
            "copy.sh",
            "# ikit:\n#   name: copy-dump\n#   runs_on: local\n#   timeout: 3600\n#   params:\n#     SRC_SSH: from.target.ssh\n#     SRC_ARGS: from.target.ssh_args\n#     DEST_SSH: to.target.ssh\n#     DEST_DIR: target.dir\nscp -3 \"$SRC_SSH:x\" \"$DEST_SSH:$DEST_DIR/\"\n",
        )
        .await;

        let e = app.script_preview(&run_req("copy-dump")).await.unwrap_err();
        assert!(e.to_string().contains("--from"), "{e}");
        let mut req = run_req("copy-dump");
        req.from = Some("wl-acme-dev-001".into());
        let e = app.script_preview(&req).await.unwrap_err();
        assert!(e.to_string().contains("same"), "{e}");

        req.from = Some(prod.clone());
        req.args = vec!["-g".into(), "platform".into()];
        let report = app.script_run(&req, "test").await.unwrap();
        assert!(report.ok);
        let (program, args, stdin) = &rec.calls()[0];
        assert_eq!(
            (program.as_str(), args.as_slice()),
            ("sh", ["-s".to_owned()].as_slice())
        );
        for line in [
            "export SRC_SSH='ubuntu@10.0.0.9'",
            "export SRC_ARGS='-o BatchMode=yes -o ConnectTimeout=10 -p 2222'",
            "export DEST_SSH='ubuntu@10.0.0.5'",
            "export DEST_DIR='.ikit/acme/wl-acme-dev-001'",
            "export IKIT_FROM_WORKLOAD='wl-acme-prod-001'",
            "export IKIT_TO_SSH='ubuntu@10.0.0.5'",
            "mktemp -d",
            "sh 'scripts/copy-dump.sh' -g platform </dev/null",
        ] {
            assert!(stdin.contains(line), "{line} missing from:\n{stdin}");
        }
        let history = app.script_history(None, Some(&prod), 5).await.unwrap();
        assert_eq!(history[0].from_workload.as_deref(), Some(prod.as_str()));
    }

    #[tokio::test]
    async fn service_runs_need_a_deployed_workload() {
        let (dir, app, rec) = setup().await;
        add(
            &app,
            &dir,
            "vacuum.sh",
            "# ikit:\n#   name: vacuum\n#   runs_on: service:postgres\n#   params:\n#     PGPASSWORD: service.postgres.env.POSTGRES_PASSWORD\npsql -c 'VACUUM'\n",
        )
        .await;
        let e = app
            .script_run(&run_req("vacuum"), "test")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("must be deployed"), "{e}");
        let runs = app.workload_runs("wl-acme-dev-001", 1).await.unwrap();
        assert_eq!(runs[0].result, RunResult::Failed);

        set_applied(&app, "wl-acme-dev-001").await;
        app.script_run(&run_req("vacuum"), "test").await.unwrap();
        let (program, args, stdin) = rec.calls().last().unwrap().clone();
        assert_eq!(program, "ssh");
        let remote = args.last().unwrap();
        assert!(
            remote.contains("docker compose -p 'wl-acme-dev-001'")
                && remote.ends_with("exec -T postgres sh -s"),
            "{remote}"
        );
        assert!(!remote.contains("pg-s3cret"));
        assert!(stdin.contains("export PGPASSWORD='pg-s3cret'"));

        // Scripts for services the workload lacks are refused.
        add(
            &app,
            &dir,
            "r.sh",
            "# ikit:\n#   name: r\n#   runs_on: service:redis\necho\n",
        )
        .await;
        let e = app.script_preview(&run_req("r")).await.unwrap_err();
        assert!(e.to_string().contains("service `redis`"), "{e}");
    }

    #[tokio::test]
    async fn script_hooks_run_in_their_phase() {
        let (dir, app, rec) = setup().await;
        let wl = "wl-acme-dev-001";
        add(&app, &dir, "create-db.sh", CREATE_DB).await;
        add(
            &app,
            &dir,
            "warm.sh",
            "# ikit:\n#   name: warm\n#   runs_on: service:postgres\necho warm\n",
        )
        .await;
        let hook = |name: &str, script: &str, phase, params: &[(&str, &str)]| NewHookRequest {
            name: name.into(),
            phase,
            fail_on_error: None,
            action: HookRequest::Script {
                script: script.into(),
                params: params
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            },
        };
        let e = app
            .hook_add(wl, hook("x", "nope", HookPhase::PostApply, &[]))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("script `nope`"), "{e}");
        let added = app
            .hook_add(
                wl,
                hook(
                    "dbs",
                    "create-db",
                    HookPhase::PostApply,
                    &[("PGSUPERUSER", "admin")],
                ),
            )
            .await
            .unwrap();
        assert_eq!(added.action.kind(), HookKind::Script);
        assert!(added.fail_on_error);
        assert_eq!(added.action.describe(), "script create-db (PGSUPERUSER)");
        app.hook_add(wl, hook("warm", "warm", HookPhase::PreApply, &[]))
            .await
            .unwrap();

        let e = app.script_remove("create-db").await.unwrap_err();
        assert!(matches!(e, AppError::InUse { .. }), "{e}");

        // A first deploy's pre-apply skips scripts that run in a service.
        let draft = app.workload_get(wl).await.unwrap();
        let target = app.target_show("vm").await.unwrap();
        let place = app.place(&target, wl).unwrap();
        let ran = app
            .run_phase(&draft, &place, HookPhase::PreApply, None, false, None)
            .await
            .unwrap();
        assert_eq!(ran.len(), 1);
        assert!(ran[0].warning.as_deref().unwrap().starts_with("skipped"));
        assert!(rec.calls().is_empty());

        set_applied(&app, wl).await;
        let ran = app.hook_run(wl, None, false, "test").await.unwrap();
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].step, "script:create-db");
        let (_, _, stdin) = rec.calls()[0].clone();
        assert!(stdin.contains("export PGSUPERUSER='admin'"), "{stdin}");
        assert!(stdin.contains("export IKIT_PHASE='post-apply'"), "{stdin}");
        let history = app
            .script_history(Some("create-db"), None, 5)
            .await
            .unwrap();
        assert_eq!(history[0].hook.as_deref(), Some("dbs"));
        assert_eq!(history[0].params["PGSUPERUSER"], "admin");
        let steps = app.hook_history(wl, Some("dbs"), 5).await.unwrap();
        assert_eq!(steps[0].script, "script:create-db");

        // A failing script fails the hook (and the run).
        *rec.code.lock().unwrap() = 1;
        let e = app
            .hook_run(wl, Some("dbs"), false, "test")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("boom"), "{e}");

        app.hook_remove(wl, "dbs").await.unwrap();
        app.script_remove("create-db").await.unwrap();
        assert!(matches!(
            app.script_show("create-db").await,
            Err(AppError::NotFound(_))
        ));
    }

    /// The whole path with a real shell, on a local target.
    #[tokio::test]
    async fn runs_for_real_on_a_local_target() {
        let (dir, app, _) = app().await;
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        let wl = app.workload_create("dev", "laptop").await.unwrap().name;
        app.services_add(
            &wl,
            vec![svc(
                "postgres",
                "16",
                Category::Data,
                &[("POSTGRES_PASSWORD", "it's $ecret")],
            )],
        )
        .await
        .unwrap();
        add(
            &app,
            &dir,
            "show.sh",
            "#!/bin/sh\n# ikit:\n#   name: show\n#   runs_on: target\n#   params:\n#     PW: service.postgres.env.POSTGRES_PASSWORD\n#     WHO: { value: tester }\necho \"$WHO $IKIT_WORKLOAD $PW $*\"\npwd\n",
        )
        .await;
        let mut req = run_req("show");
        req.workload = wl.clone();
        req.args = vec!["a b".into()];
        let report = app.script_run(&req, "test").await.unwrap();
        assert!(report.ok, "{report:?}");
        assert!(
            report
                .stdout
                .starts_with(&format!("tester {wl} it's $ecret a b\n")),
            "{}",
            report.stdout
        );
        let deploy_dir = app.org().dir.join("deploy").join(&wl);
        assert!(report.stdout.contains(&deploy_dir.display().to_string()));
        assert!(deploy_dir.join("scripts/show.sh").exists());
    }
}
