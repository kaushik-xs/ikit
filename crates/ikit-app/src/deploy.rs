//! Deploying workloads to their target with Docker Compose: plan, apply, status, destroy.
//!
//! The compose project for a workload lives in `orgs/<org>/deploy/<workload>/` for a local
//! target and in `~/.ikit/<org>/<workload>/` on an SSH target. Those files are the record of
//! what was last applied, so `plan` diffs against what the target actually has.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use ikit_core::{
    Connection, HookPhase, Run, RunAction, RunResult, Target, Workload, WorkloadStatus,
};
use ikit_provider::compose::{
    self, COMPOSE_FILE, Plan, Rendered, SECRETS_FILE, ServiceInput, WorkloadInput,
};
use ikit_state::NewRun;
use serde::{Deserialize, Serialize};

use crate::hooks::{HookOutcome, HookPlan};
use crate::targets::ssh_args;
use crate::{App, AppError, ExecOutput, Result};

pub(crate) const QUICK: Duration = Duration::from_secs(60);
/// `docker compose up --wait` pulls images and waits for health checks.
const DEPLOY: Duration = Duration::from_secs(15 * 60);
/// A lock older than this belongs to a run that died.
const STALE_LOCK: chrono::Duration = chrono::Duration::minutes(30);

#[derive(Debug, Clone, Serialize)]
pub struct DeployPlan {
    pub workload: String,
    pub target: String,
    /// The workload's status before this plan; a failed apply is retried even without changes.
    pub status: WorkloadStatus,
    pub plan: Plan,
    /// Hook scripts and whether they would run.
    pub hooks: Vec<HookPlan>,
    /// Reasons `apply` will refuse to run, e.g. unpinned images in a prod env.
    pub blockers: Vec<String>,
    #[serde(skip)]
    pub rendered: Rendered,
}

impl DeployPlan {
    /// Whether `apply` would do anything.
    pub fn needs_apply(&self) -> bool {
        !self.plan.is_empty()
            || self.status != WorkloadStatus::Applied
            || self
                .hooks
                .iter()
                .any(|h| h.pending() > 0 || !h.changed().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerStatus {
    pub service: String,
    #[serde(default)]
    pub image: String,
    pub state: String,
    pub health: String,
    pub status: String,
    pub ports: Vec<String>,
    /// Compose's hash of the service config the container was created from.
    #[serde(skip)]
    pub config_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub run_id: i64,
    pub plan: DeployPlan,
    pub containers: Vec<ContainerStatus>,
    /// Hook steps that ran (pre-apply and post-apply).
    pub hooks_ran: Vec<HookOutcome>,
}

/// Where a workload's compose project lives.
pub(crate) enum Place {
    Local(PathBuf),
    Ssh { args: Vec<String>, dir: String },
}

/// Single-quotes `s` for a POSIX shell.
pub(crate) fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `s` as one shell word: unchanged when it is plainly safe, single-quoted otherwise.
fn sh_word(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,".contains(c));
    if safe { s.to_owned() } else { sh(s) }
}

pub(crate) fn failed(what: &str, out: &ExecOutput) -> AppError {
    let tail: Vec<&str> = out
        .stderr
        .lines()
        .chain(out.stdout.lines())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail = tail[tail.len().saturating_sub(15)..].join("\n");
    let detail = if out.timed_out {
        "timed out".to_owned()
    } else {
        tail
    };
    AppError::Invalid(format!("{what} failed: {detail}"))
}

/// `docker compose ps --format json` prints one object per line (newer) or one array (older).
pub(crate) fn parse_ps(stdout: &str) -> Vec<ContainerStatus> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Publisher {
        #[serde(rename = "URL", default)]
        url: String,
        target_port: u16,
        published_port: u16,
        protocol: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Ps {
        service: String,
        #[serde(default)]
        image: String,
        state: String,
        #[serde(default)]
        health: String,
        /// `k=v,k=v`; label values may contain commas, so look keys up by prefix.
        #[serde(default)]
        labels: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        publishers: Option<Vec<Publisher>>,
    }
    let trimmed = stdout.trim();
    let rows: Vec<Ps> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).unwrap_or_default()
    } else {
        trimmed
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    };
    let mut out: Vec<ContainerStatus> = rows
        .into_iter()
        .map(|r| ContainerStatus {
            ports: r
                .publishers
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p.published_port != 0)
                .map(|p| {
                    format!(
                        "{}:{}->{}/{}",
                        p.url, p.published_port, p.target_port, p.protocol
                    )
                })
                .collect(),
            config_hash: r
                .labels
                .split(',')
                .find_map(|l| l.strip_prefix("com.docker.compose.config-hash="))
                .unwrap_or_default()
                .to_owned(),
            service: r.service,
            image: r.image,
            state: r.state,
            health: r.health,
            status: r.status,
        })
        .collect();
    out.sort_by(|a, b| a.service.cmp(&b.service));
    out
}

impl App {
    pub(crate) fn place(&self, target: &Target, workload: &str) -> Result<Place> {
        match &target.connection {
            Connection::Local => Ok(Place::Local(self.org.dir.join("deploy").join(workload))),
            Connection::Ssh { .. } => Ok(Place::Ssh {
                args: ssh_args(&target.connection),
                dir: format!(".ikit/{}/{workload}", self.org.name),
            }),
            Connection::K8s { .. } => Err(AppError::Invalid(format!(
                "target `{}` is a Kubernetes cluster; deploying to clusters arrives in Phase 5 (use a VM target for now)",
                target.name
            ))),
        }
    }

    async fn ssh(
        &self,
        args: &[String],
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput> {
        let mut all = args.to_vec();
        all.push(script.to_owned());
        Ok(self.exec.run("ssh", &all, stdin, timeout).await?)
    }

    pub(crate) async fn read_file(&self, place: &Place, name: &str) -> Result<Option<String>> {
        match place {
            Place::Local(dir) => match std::fs::read_to_string(dir.join(name)) {
                Ok(s) => Ok(Some(s)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            Place::Ssh { args, dir } => {
                let path = sh(&format!("{dir}/{name}"));
                let out = self
                    .ssh(
                        args,
                        &format!("if [ -f {path} ]; then cat {path}; else exit 3; fi"),
                        None,
                        QUICK,
                    )
                    .await?;
                match out.code {
                    Some(0) => Ok(Some(out.stdout)),
                    Some(3) => Ok(None),
                    _ => Err(failed("reading the applied files over ssh", &out)),
                }
            }
        }
    }

    /// Writes the compose files. Both are owner-only; the secrets file must be.
    async fn write_files(&self, place: &Place, rendered: &Rendered) -> Result<()> {
        let secrets = if rendered.secrets.is_empty() {
            "services: {}\n".to_owned()
        } else {
            rendered.secrets.clone()
        };
        let files = [
            (COMPOSE_FILE, rendered.compose.as_str()),
            (SECRETS_FILE, secrets.as_str()),
        ];
        match place {
            Place::Local(dir) => {
                std::fs::create_dir_all(dir)?;
                for (name, content) in files {
                    write_private(&dir.join(name), content)?;
                }
            }
            Place::Ssh { args, dir } => {
                for (name, content) in files {
                    let script =
                        format!("umask 077 && mkdir -p {d} && cat > {d}/{name}", d = sh(dir));
                    let out = self
                        .ssh(args, &script, Some(content.as_bytes()), QUICK)
                        .await?;
                    if !out.success() {
                        return Err(failed(&format!("writing {name} over ssh"), &out));
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn compose(
        &self,
        place: &Place,
        project: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput> {
        self.compose_with(place, project, args, None, timeout).await
    }

    /// `docker compose` for the workload's project, locally or over ssh, feeding `stdin`.
    pub(crate) async fn compose_with(
        &self,
        place: &Place,
        project: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput> {
        match place {
            Place::Local(dir) => {
                let d = dir.display().to_string();
                let mut all: Vec<String> = vec![
                    "compose".into(),
                    "--project-directory".into(),
                    d.clone(),
                    "-p".into(),
                    project.into(),
                ];
                for f in [COMPOSE_FILE, SECRETS_FILE] {
                    all.extend(["-f".to_owned(), format!("{d}/{f}")]);
                }
                all.extend(args.iter().map(|a| a.to_string()));
                Ok(self.exec.run("docker", &all, stdin, timeout).await?)
            }
            Place::Ssh { args: ssh, dir } => {
                let script = format!(
                    "cd {} && docker compose -p {} -f {COMPOSE_FILE} -f {SECRETS_FILE} {}",
                    sh(dir),
                    sh(project),
                    args.iter()
                        .map(|a| sh_word(a))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                self.ssh(ssh, &script, stdin, timeout).await
            }
        }
    }

    async fn docker_login(
        &self,
        place: &Place,
        host: &str,
        username: &str,
        password: &str,
    ) -> Result<()> {
        let out = match place {
            Place::Local(_) => {
                let args: Vec<String> = ["login", host, "--username", username, "--password-stdin"]
                    .map(str::to_owned)
                    .to_vec();
                self.exec
                    .run("docker", &args, Some(password.as_bytes()), QUICK)
                    .await?
            }
            Place::Ssh { args, .. } => {
                let script = format!(
                    "docker login {} --username {} --password-stdin",
                    sh(host),
                    sh(username)
                );
                self.ssh(args, &script, Some(password.as_bytes()), QUICK)
                    .await?
            }
        };
        if !out.success() {
            return Err(failed(&format!("docker login to {host}"), &out));
        }
        Ok(())
    }

    /// The workload's compose files, with secret values read from the keychain.
    pub async fn render_workload(&self, workload: &str) -> Result<Rendered> {
        let detail = self.workload_show(workload).await?;
        let mut specs = BTreeMap::new();
        for entry in self.store.registry().list().await? {
            specs.extend(entry.versions.into_iter().map(|v| (v.id, v.spec)));
        }
        let mut services = Vec::new();
        for instance in detail.services {
            let secrets: BTreeMap<String, String> = if instance.secret_env.is_empty() {
                BTreeMap::new()
            } else {
                let key = self.service_secret_key(workload, &instance.name);
                let json = self.secrets.get(&key)?.ok_or_else(|| {
                    AppError::Invalid(format!(
                        "keychain entry {key} is missing; set the secrets again with `ikit svc set {} --secret ...`",
                        instance.name
                    ))
                })?;
                serde_json::from_str(&json).map_err(|e| {
                    AppError::Invalid(format!("keychain entry {key} is not valid JSON: {e}"))
                })?
            };
            let spec = specs.remove(&instance.version_id).unwrap_or_default();
            services.push(ServiceInput {
                instance,
                spec,
                secrets,
            });
        }
        let input = WorkloadInput {
            org: self.org.name.clone(),
            workload: workload.to_owned(),
            services,
        };
        compose::render(&input).map_err(|e| AppError::Invalid(e.to_string()))
    }

    /// What `apply` would change on the target.
    pub async fn deploy_plan(&self, workload: &str) -> Result<DeployPlan> {
        let wl = self.workload_get(workload).await?;
        let target = self.target_show(&wl.target).await?;
        let place = self.place(&target, workload)?;
        let rendered = self.render_workload(workload).await?;
        let applied_compose = self.read_file(&place, COMPOSE_FILE).await?;
        let applied_secrets = self
            .read_file(&place, SECRETS_FILE)
            .await?
            .unwrap_or_default();
        let plan = compose::diff(
            applied_compose
                .as_deref()
                .map(|c| (c, applied_secrets.as_str())),
            &rendered,
        )
        .map_err(|e| {
            AppError::Invalid(format!(
                "the applied compose file on {} is unreadable: {e}",
                target.name
            ))
        })?;
        let hooks = self.hook_plans(workload, wl.status, &place).await?;
        let mut blockers = Vec::new();
        let unpinned = self.unpinned_for_env(workload).await?;
        if !unpinned.is_empty() {
            blockers.push(format!(
                "`{}` deploys need digest-pinned images; pin {} with `ikit registry version pin <service> <version>`",
                wl.env,
                unpinned.join(", ")
            ));
        }
        Ok(DeployPlan {
            workload: workload.to_owned(),
            target: target.name,
            status: wl.status,
            plan,
            hooks,
            blockers,
            rendered,
        })
    }

    pub(crate) async fn lock(&self, wl: &Workload, holder: &str) -> Result<()> {
        let stale = chrono::Utc::now() - STALE_LOCK;
        if let Some(by) = self
            .store
            .workloads()
            .try_lock(wl.id, holder, stale)
            .await?
        {
            return Err(AppError::Invalid(format!(
                "{} is locked by another run ({by}); try again when it finishes",
                wl.name
            )));
        }
        Ok(())
    }

    /// Applies `expected` (from [`App::deploy_plan`]). Refused if the plan changed since.
    /// `allow_changed` re-runs hook scripts that changed after being applied.
    pub async fn deploy_apply(
        &self,
        expected: &DeployPlan,
        actor: &str,
        allow_changed: bool,
    ) -> Result<ApplyReport> {
        let wl = self.workload_get(&expected.workload).await?;
        let holder = format!("{actor} (pid {})", std::process::id());
        self.lock(&wl, &holder).await?;
        let result = self.apply_locked(&wl, expected, actor, allow_changed).await;
        self.store.workloads().unlock(wl.id, &holder).await?;
        result
    }

    async fn apply_locked(
        &self,
        wl: &Workload,
        expected: &DeployPlan,
        actor: &str,
        allow_changed: bool,
    ) -> Result<ApplyReport> {
        let plan = self.deploy_plan(&wl.name).await?;
        if let Some(b) = plan.blockers.first() {
            return Err(AppError::Invalid(b.clone()));
        }
        if plan.plan != expected.plan || plan.rendered != expected.rendered {
            return Err(AppError::Invalid(format!(
                "{} changed since it was planned; run `ikit plan` and apply again",
                wl.name
            )));
        }
        let target = self.target_show(&wl.target).await?;
        let place = self.place(&target, &wl.name)?;
        let plan_json = serde_json::to_string(&plan.plan).expect("plan serializes");
        let run = NewRun {
            action: RunAction::Apply,
            actor,
            plan_json: &plan_json,
            compose_yaml: Some(&plan.rendered.compose),
        };
        let run_id = self.store.workloads().start_run(wl.id, run).await?;

        let outcome = async {
            let mut ran = self
                .run_phase(
                    wl,
                    &place,
                    HookPhase::PreApply,
                    Some(run_id),
                    allow_changed,
                    None,
                )
                .await?;
            self.write_files(&place, &plan.rendered).await?;
            let logins = self.store.registry().logins().await?;
            let hosts: std::collections::BTreeSet<String> = plan
                .rendered
                .images
                .iter()
                .map(|i| compose::image_registry(i))
                .collect();
            for login in logins.iter().filter(|l| hosts.contains(&l.host)) {
                let password = self.registry_password(&login.host)?;
                self.docker_login(&place, &login.host, &login.username, &password)
                    .await?;
            }
            let args = [
                "up",
                "--detach",
                "--remove-orphans",
                "--wait",
                "--wait-timeout",
                "600",
            ];
            let out = self.compose(&place, &wl.name, &args, DEPLOY).await?;
            if !out.success() {
                return Err(failed("docker compose up", &out));
            }
            // Post-apply hooks see the workload as applied (services are up).
            let up = Workload {
                status: WorkloadStatus::Applied,
                ..wl.clone()
            };
            ran.extend(
                self.run_phase(
                    &up,
                    &place,
                    HookPhase::PostApply,
                    Some(run_id),
                    allow_changed,
                    None,
                )
                .await?,
            );
            Ok(ran)
        }
        .await;

        match outcome {
            Ok(hooks_ran) => {
                self.store
                    .workloads()
                    .set_status(wl.id, WorkloadStatus::Applied)
                    .await?;
                self.store
                    .workloads()
                    .finish_run(run_id, RunResult::Ok, None)
                    .await?;
                let containers = self.deploy_status(&wl.name).await.unwrap_or_default();
                Ok(ApplyReport {
                    run_id,
                    plan,
                    containers,
                    hooks_ran,
                })
            }
            Err(e) => {
                self.store
                    .workloads()
                    .set_status(wl.id, WorkloadStatus::Failed)
                    .await?;
                self.store
                    .workloads()
                    .finish_run(run_id, RunResult::Failed, Some(&e.to_string()))
                    .await?;
                Err(e)
            }
        }
    }

    /// Containers of the workload on its target (empty if nothing is deployed).
    pub async fn deploy_status(&self, workload: &str) -> Result<Vec<ContainerStatus>> {
        let wl = self.workload_get(workload).await?;
        let target = self.target_show(&wl.target).await?;
        let place = self.place(&target, workload)?;
        if self.read_file(&place, COMPOSE_FILE).await?.is_none() {
            return Ok(vec![]);
        }
        let out = self
            .compose(
                &place,
                workload,
                &["ps", "--all", "--format", "json"],
                QUICK,
            )
            .await?;
        if !out.success() {
            return Err(failed("docker compose ps", &out));
        }
        Ok(parse_ps(&out.stdout))
    }

    /// Stops and removes the workload's containers, and its volumes (data) if `volumes`.
    /// The workload definition stays, back in `draft`.
    pub async fn deploy_destroy(&self, workload: &str, volumes: bool, actor: &str) -> Result<i64> {
        let wl = self.workload_get(workload).await?;
        let holder = format!("{actor} (pid {})", std::process::id());
        self.lock(&wl, &holder).await?;
        let result = async {
            let target = self.target_show(&wl.target).await?;
            let place = self.place(&target, workload)?;
            let plan_json = serde_json::json!({ "volumes": volumes }).to_string();
            let run = NewRun {
                action: RunAction::Destroy,
                actor,
                plan_json: &plan_json,
                compose_yaml: None,
            };
            let run_id = self.store.workloads().start_run(wl.id, run).await?;
            let outcome = async {
                if self.read_file(&place, COMPOSE_FILE).await?.is_some() {
                    for o in self
                        .run_phase(
                            &wl,
                            &place,
                            HookPhase::PreDestroy,
                            Some(run_id),
                            false,
                            None,
                        )
                        .await?
                    {
                        if let Some(w) = o.warning {
                            tracing::warn!(hook = %o.hook, "{w}");
                        }
                    }
                    let mut args = vec!["down", "--remove-orphans"];
                    if volumes {
                        args.push("--volumes");
                    }
                    let out = self.compose(&place, workload, &args, DEPLOY).await?;
                    if !out.success() {
                        return Err(failed("docker compose down", &out));
                    }
                }
                match &place {
                    Place::Local(dir) => match std::fs::remove_dir_all(dir) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                        _ => {}
                    },
                    Place::Ssh { args, dir } => {
                        let out = self
                            .ssh(args, &format!("rm -rf {}", sh(dir)), None, QUICK)
                            .await?;
                        if !out.success() {
                            return Err(failed("removing the compose files over ssh", &out));
                        }
                    }
                }
                Ok(())
            }
            .await;
            match outcome {
                Ok(()) => {
                    self.store
                        .workloads()
                        .set_status(wl.id, WorkloadStatus::Draft)
                        .await?;
                    self.store
                        .workloads()
                        .finish_run(run_id, RunResult::Ok, None)
                        .await?;
                    Ok(run_id)
                }
                Err(e) => {
                    self.store
                        .workloads()
                        .set_status(wl.id, WorkloadStatus::Failed)
                        .await?;
                    self.store
                        .workloads()
                        .finish_run(run_id, RunResult::Failed, Some(&e.to_string()))
                        .await?;
                    Err(e)
                }
            }
        }
        .await;
        self.store.workloads().unlock(wl.id, &holder).await?;
        result
    }

    pub async fn workload_runs(&self, workload: &str, limit: i64) -> Result<Vec<Run>> {
        let wl = self.workload_get(workload).await?;
        Ok(self.store.workloads().runs(wl.id, limit).await?)
    }
}

/// Writes `content` readable by the owner only (0600 on Unix).
pub(crate) fn write_private(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(0o600);
        let mut f = opts.open(path)?;
        // `mode` only applies to new files; tighten files that already existed.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(content.as_bytes())
    }
    #[cfg(not(unix))]
    {
        opts.open(path)?.write_all(content.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ikit_core::Category;

    use super::*;
    use crate::testing::app;
    use crate::{Exec, ServiceRequest};

    /// Answers docker commands like a healthy daemon and records them.
    #[derive(Default)]
    struct FakeDocker {
        calls: Mutex<Vec<String>>,
        fail_up: Mutex<VecDeque<bool>>,
        /// Rows of `_ikit_hook_ledger`: (hook, script) → checksum.
        ledger: Mutex<BTreeMap<(String, String), String>>,
        /// SQL piped into `exec` calls.
        sql: Mutex<Vec<String>>,
    }

    impl FakeDocker {
        /// Plays the database for `compose exec ... psql`: answers ledger reads, records
        /// script runs, and fails scripts containing `FAIL`.
        fn exec_sql(&self, input: &str) -> ExecOutput {
            self.sql.lock().unwrap().push(input.to_owned());
            let ok = |stdout: String| ExecOutput {
                code: Some(0),
                stdout,
                ..Default::default()
            };
            if let Some(rest) = input.split("WHERE hook = '").nth(1) {
                let hook = rest.split('\'').next().unwrap();
                let rows: Vec<String> = self
                    .ledger
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|((h, _), _)| h == hook)
                    .map(|((_, s), c)| format!("{s}|{c}"))
                    .collect();
                return ok(rows.join("\n"));
            }
            if input.contains("FAIL") {
                return ExecOutput {
                    code: Some(3),
                    stderr: "ERROR:  syntax error at or near \"FAIL\"".into(),
                    ..Default::default()
                };
            }
            if let Some(values) = input.split("VALUES ('").nth(1) {
                let parts: Vec<&str> = values.split('\'').collect();
                let (hook, script, sum) = (parts[0], parts[2], parts[4]);
                self.ledger
                    .lock()
                    .unwrap()
                    .insert((hook.into(), script.into()), sum.into());
            }
            ok(String::new())
        }
    }

    #[async_trait]
    impl Exec for FakeDocker {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            stdin: Option<&[u8]>,
            _: Duration,
        ) -> io::Result<ExecOutput> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push(line.clone());
            if line.contains("pg_dump") {
                let dump = b"PGDMP\x00binary\xff".to_vec();
                return Ok(ExecOutput {
                    code: Some(0),
                    raw_stdout: dump,
                    ..Default::default()
                });
            }
            if line.contains("pg_restore") {
                self.sql
                    .lock()
                    .unwrap()
                    .push(format!("RESTORE {:?}", stdin.unwrap_or_default()));
                return Ok(ExecOutput {
                    code: Some(0),
                    ..Default::default()
                });
            }
            if line.contains(" exec -T ") && line.contains("FAIL") {
                return Ok(ExecOutput {
                    code: Some(127),
                    stderr: "sh: syntax error: FAIL".into(),
                    ..Default::default()
                });
            }
            if line.contains(" exec -T ") {
                return Ok(self.exec_sql(&String::from_utf8_lossy(stdin.unwrap_or_default())));
            }
            let ok = |stdout: &str| {
                Ok(ExecOutput {
                    code: Some(0),
                    stdout: stdout.into(),
                    ..Default::default()
                })
            };
            if line.contains(" up ") && self.fail_up.lock().unwrap().pop_front().unwrap_or(false) {
                return Ok(ExecOutput {
                    code: Some(1),
                    stderr: "container redis is unhealthy".into(),
                    ..Default::default()
                });
            }
            if line.contains(" ps ") {
                return ok(
                    r#"{"Service":"redis","State":"running","Health":"healthy","Status":"Up 3 seconds","Publishers":[{"URL":"0.0.0.0","TargetPort":6379,"PublishedPort":16379,"Protocol":"tcp"}]}"#,
                );
            }
            ok("")
        }
    }

    async fn setup() -> (tempfile::TempDir, App, Arc<FakeDocker>) {
        let (dir, app, _) = app().await;
        let docker = Arc::new(FakeDocker::default());
        let app = app.with_exec(docker.clone());
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        let req = ServiceRequest {
            name: None,
            registry: "redis".into(),
            constraint: "7".parse().unwrap(),
            group: Category::Data,
            env: BTreeMap::new(),
        };
        app.services_add("wl-acme-dev-001", vec![req])
            .await
            .unwrap();
        (dir, app, docker)
    }

    #[tokio::test]
    async fn plan_apply_then_nothing_to_do() {
        let (_dir, app, docker) = setup().await;
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        assert!(plan.plan.first_apply && plan.needs_apply());

        let report = app.deploy_apply(&plan, "test", false).await.unwrap();
        assert_eq!(report.containers[0].ports, ["0.0.0.0:16379->6379/tcp"]);
        let dir = app.org().dir.join("deploy/wl-acme-dev-001");
        assert!(dir.join(COMPOSE_FILE).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(SECRETS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let calls = docker.calls.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("-p wl-acme-dev-001") && c.contains(" up --detach")),
            "{calls:?}"
        );
        assert_eq!(
            app.workload_get("wl-acme-dev-001").await.unwrap().status,
            WorkloadStatus::Applied
        );

        let again = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        assert!(!again.needs_apply(), "{:?}", again.plan);
        assert_eq!(
            app.workload_runs("wl-acme-dev-001", 5).await.unwrap()[0].result,
            RunResult::Ok
        );
    }

    #[tokio::test]
    async fn stale_plan_is_refused() {
        let (_dir, app, _) = setup().await;
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        let publish = vec!["16379:6379".parse().unwrap()];
        app.service_publish("wl-acme-dev-001", "redis", publish, &[])
            .await
            .unwrap();
        let err = app.deploy_apply(&plan, "test", false).await.unwrap_err();
        assert!(
            err.to_string().contains("changed since it was planned"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn failed_apply_is_recorded_and_retried() {
        let (_dir, app, docker) = setup().await;
        docker.fail_up.lock().unwrap().push_back(true);
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        let err = app.deploy_apply(&plan, "test", false).await.unwrap_err();
        assert!(err.to_string().contains("unhealthy"), "{err}");
        assert_eq!(
            app.workload_get("wl-acme-dev-001").await.unwrap().status,
            WorkloadStatus::Failed
        );

        // Files are already on the target, but a failed workload still needs an apply.
        let retry = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        assert!(retry.plan.is_empty() && retry.needs_apply());
        app.deploy_apply(&retry, "test", false).await.unwrap();
        let runs = app.workload_runs("wl-acme-dev-001", 5).await.unwrap();
        assert_eq!(
            runs.iter().map(|r| r.result).collect::<Vec<_>>(),
            [RunResult::Ok, RunResult::Failed]
        );
    }

    #[tokio::test]
    async fn destroy_removes_project_and_returns_to_draft() {
        let (_dir, app, docker) = setup().await;
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        app.deploy_apply(&plan, "test", false).await.unwrap();
        assert!(
            app.workload_remove("wl-acme-dev-001").await.is_err(),
            "applied workloads must be destroyed first"
        );

        app.deploy_destroy("wl-acme-dev-001", false, "test")
            .await
            .unwrap();
        assert!(!app.org().dir.join("deploy/wl-acme-dev-001").exists());
        assert_eq!(
            app.workload_get("wl-acme-dev-001").await.unwrap().status,
            WorkloadStatus::Draft
        );
        let calls = docker.calls.lock().unwrap().clone();
        assert!(
            calls.iter().any(|c| c.ends_with("down --remove-orphans")),
            "{calls:?}"
        );
        app.workload_remove("wl-acme-dev-001").await.unwrap();
    }

    #[tokio::test]
    async fn kubernetes_targets_are_not_deployable_yet() {
        let (_dir, app, _) = app().await;
        app.target_attach(
            "kind",
            Connection::K8s {
                context: "kind-kind".into(),
                kubeconfig: None,
            },
        )
        .await
        .unwrap();
        app.workload_create("dev", "kind").await.unwrap();
        let req = ServiceRequest {
            name: None,
            registry: "redis".into(),
            constraint: "latest".parse().unwrap(),
            group: Category::Data,
            env: BTreeMap::new(),
        };
        app.services_add("wl-acme-dev-001", vec![req])
            .await
            .unwrap();
        assert!(
            app.deploy_plan("wl-acme-dev-001")
                .await
                .unwrap_err()
                .to_string()
                .contains("Phase 5")
        );
    }

    #[tokio::test]
    async fn ssh_targets_get_files_and_compose_over_ssh() {
        let (_dir, app, _) = app().await;
        let docker = Arc::new(FakeDocker::default());
        let app = app.with_exec(docker.clone());
        let conn = Connection::Ssh {
            host: "10.0.0.5".into(),
            user: Some("ubuntu".into()),
            port: None,
            identity_file: None,
        };
        app.target_attach("edge", conn).await.unwrap();
        app.workload_create("dev", "edge").await.unwrap();
        let req = ServiceRequest {
            name: None,
            registry: "redis".into(),
            constraint: "7".parse().unwrap(),
            group: Category::Data,
            env: BTreeMap::new(),
        };
        app.services_add("wl-acme-dev-001", vec![req])
            .await
            .unwrap();
        app.registry_login("docker.io", "bot", "token")
            .await
            .unwrap();

        // The fake answers `cat` with an empty file, so this is a plan against an empty project.
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        app.deploy_apply(&plan, "test", false).await.unwrap();

        let calls = docker.calls.lock().unwrap().clone();
        let dest = "ssh -o BatchMode=yes -o ConnectTimeout=10 ubuntu@10.0.0.5";
        assert!(calls.iter().all(|c| c.starts_with(dest)), "{calls:#?}");
        let dir = "'.ikit/acme/wl-acme-dev-001'";
        assert!(
            calls.iter().any(|c| c.ends_with(&format!(
                "umask 077 && mkdir -p {dir} && cat > {dir}/secrets.compose.yaml"
            ))),
            "{calls:#?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.ends_with("docker login 'docker.io' --username 'bot' --password-stdin")),
            "{calls:#?}"
        );
        let up = calls.iter().find(|c| c.contains(" up ")).unwrap();
        assert!(up.contains(&format!("cd {dir} && docker compose -p 'wl-acme-dev-001' -f compose.yaml -f secrets.compose.yaml up --detach")), "{up}");
    }

    fn sql_hook(name: &str, service: &str, source: std::path::PathBuf) -> crate::NewHookRequest {
        crate::NewHookRequest {
            name: name.into(),
            phase: HookPhase::PostApply,
            fail_on_error: None,
            action: crate::HookRequest::Sql {
                service: service.into(),
                engine: None,
                database: None,
                source,
                via: ikit_core::SqlVia::Exec,
            },
        }
    }

    fn shell_hook(
        name: &str,
        phase: HookPhase,
        command: &str,
        service: Option<&str>,
    ) -> crate::NewHookRequest {
        crate::NewHookRequest {
            name: name.into(),
            phase,
            fail_on_error: None,
            action: crate::HookRequest::Shell {
                command: command.into(),
                service: service.map(Into::into),
            },
        }
    }

    #[tokio::test]
    async fn shell_hooks_run_in_their_phase() {
        let (_dir, app, docker) = setup().await;
        let wl = "wl-acme-dev-001";
        app.hook_add(
            wl,
            shell_hook("warm", HookPhase::PreApply, "echo warm", Some("redis")),
        )
        .await
        .unwrap();
        app.hook_add(
            wl,
            shell_hook(
                "smoke",
                HookPhase::PostApply,
                "redis-cli ping",
                Some("redis"),
            ),
        )
        .await
        .unwrap();
        app.hook_add(
            wl,
            shell_hook(
                "bye",
                HookPhase::PreDestroy,
                "echo bye $IKIT_WORKLOAD",
                None,
            ),
        )
        .await
        .unwrap();

        let plan = app.deploy_plan(wl).await.unwrap();
        let report = app.deploy_apply(&plan, "test", false).await.unwrap();
        let steps: Vec<(&str, bool)> = report
            .hooks_ran
            .iter()
            .map(|o| (o.hook.as_str(), o.warning.is_some()))
            .collect();
        // On the first deploy the pre-apply in-service hook is skipped (nothing is running).
        assert_eq!(steps, [("warm", true), ("smoke", false)]);

        let calls = docker.calls.lock().unwrap().clone();
        let smoke = calls.iter().find(|c| c.contains("redis-cli ping")).unwrap();
        assert!(
            smoke.contains("exec -T -e IKIT_ENV=dev")
                && smoke.contains("-e IKIT_WORKLOAD=wl-acme-dev-001"),
            "{smoke}"
        );

        // Second apply: pre-apply runs now that redis is up.
        let plan = app.deploy_plan(wl).await.unwrap();
        let report = app.deploy_apply(&plan, "test", false).await.unwrap();
        assert!(
            report.hooks_ran.iter().all(|o| o.warning.is_none()),
            "{:?}",
            report.hooks_ran
        );

        app.deploy_destroy(wl, false, "test").await.unwrap();
        let calls = docker.calls.lock().unwrap().clone();
        let bye = calls
            .iter()
            .position(|c| c.starts_with("env ") && c.contains("echo bye"))
            .expect("pre-destroy ran on the host");
        let down = calls
            .iter()
            .position(|c| c.ends_with("down --remove-orphans"))
            .unwrap();
        assert!(bye < down, "pre-destroy runs before down");
    }

    #[tokio::test]
    async fn failing_hooks_follow_their_policy() {
        let (_dir, app, _) = setup().await;
        let wl = "wl-acme-dev-001";
        // The fake answers `exec` with SQL semantics; a shell command containing FAIL fails.
        let mut soft = shell_hook("soft", HookPhase::PostApply, "FAIL soft", Some("redis"));
        soft.fail_on_error = Some(false);
        app.hook_add(wl, soft).await.unwrap();
        let plan = app.deploy_plan(wl).await.unwrap();
        let report = app.deploy_apply(&plan, "test", false).await.unwrap();
        assert!(
            report.hooks_ran[0]
                .warning
                .as_deref()
                .unwrap()
                .contains("syntax error"),
            "{:?}",
            report.hooks_ran
        );

        app.hook_add(
            wl,
            shell_hook("hard", HookPhase::PostApply, "FAIL hard", Some("redis")),
        )
        .await
        .unwrap();
        let plan = app.deploy_plan(wl).await.unwrap();
        let err = app.deploy_apply(&plan, "test", false).await.unwrap_err();
        assert!(
            err.to_string().contains("hook `hard` (shell) failed"),
            "{err}"
        );
        assert_eq!(
            app.workload_get(wl).await.unwrap().status,
            WorkloadStatus::Failed
        );
    }

    #[tokio::test]
    async fn http_hooks_keep_their_url_in_the_keychain() {
        let (_dir, app, secrets) = app().await;
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        let req = crate::NewHookRequest {
            name: "slack".into(),
            phase: HookPhase::PostApply,
            fail_on_error: None,
            action: crate::HookRequest::Http {
                method: "post".into(),
                url: "https://hooks.slack.com/services/T0/B0/secret".into(),
                body: Some(r#"{"text":"{{workload}} applied"}"#.into()),
                headers: BTreeMap::new(),
            },
        };
        let hook = app.hook_add("wl-acme-dev-001", req).await.unwrap();
        assert!(!hook.fail_on_error, "http hooks warn by default");
        let stored = serde_json::to_string(&hook).unwrap();
        assert!(
            !stored.contains("secret") && stored.contains("https://hooks.slack.com"),
            "{stored}"
        );
        let key = "org/acme/hook/wl-acme-dev-001/slack";
        assert!(
            crate::SecretStore::get(secrets.as_ref(), key)
                .unwrap()
                .unwrap()
                .ends_with("/secret")
        );

        app.workload_remove("wl-acme-dev-001").await.unwrap();
        assert_eq!(
            crate::SecretStore::get(secrets.as_ref(), key).unwrap(),
            None,
            "removed with the workload"
        );
    }

    async fn setup_postgres() -> (tempfile::TempDir, App, Arc<FakeDocker>, tempfile::TempDir) {
        let (dir, app, _) = app().await;
        let docker = Arc::new(FakeDocker::default());
        let app = app.with_exec(docker.clone());
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        let req = ServiceRequest {
            name: None,
            registry: "postgres".into(),
            constraint: "16".parse().unwrap(),
            group: Category::Data,
            env: BTreeMap::from([("POSTGRES_PASSWORD".to_owned(), "pw".to_owned())]),
        };
        app.services_add("wl-acme-dev-001", vec![req])
            .await
            .unwrap();
        let sql = tempfile::tempdir().unwrap();
        std::fs::write(
            sql.path().join("001_init.sql"),
            "CREATE TABLE orders (id int);",
        )
        .unwrap();
        std::fs::write(
            sql.path().join("R__views.sql"),
            "CREATE OR REPLACE VIEW v AS SELECT 1;",
        )
        .unwrap();
        let hook = sql_hook("schema", "postgres", sql.path().to_path_buf());
        app.hook_add("wl-acme-dev-001", hook).await.unwrap();
        (dir, app, docker, sql)
    }

    #[tokio::test]
    async fn hooks_run_once_after_apply_and_repeatables_rerun() {
        let (_dir, app, docker, sql) = setup_postgres().await;
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        assert_eq!(plan.hooks[0].pending(), 2);

        let report = app.deploy_apply(&plan, "test", false).await.unwrap();
        let ran: Vec<&str> = report.hooks_ran.iter().map(|r| r.step.as_str()).collect();
        assert_eq!(ran, ["001_init.sql", "R__views.sql"]);
        let piped = docker.sql.lock().unwrap().clone();
        assert!(
            piped[0].starts_with("CREATE TABLE IF NOT EXISTS _ikit_hook_ledger"),
            "ledger created before running"
        );
        let calls = docker.calls.lock().unwrap().clone();
        assert!(calls.iter().any(|c| c.contains("exec -T postgres sh -c exec psql -X -q -v ON_ERROR_STOP=1 -1")), "{calls:#?}");

        // Nothing left to do.
        let again = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        assert!(!again.needs_apply(), "{:?}", again.hooks);

        // A repeatable script reruns when changed; a new script runs; nothing else does.
        std::fs::write(
            sql.path().join("R__views.sql"),
            "CREATE OR REPLACE VIEW v AS SELECT 2;",
        )
        .unwrap();
        std::fs::write(
            sql.path().join("002_more.sql"),
            "ALTER TABLE orders ADD total int;",
        )
        .unwrap();
        let ran = app
            .hook_run("wl-acme-dev-001", None, false, "test")
            .await
            .unwrap();
        let ran: Vec<(&str, Option<ikit_core::ScriptStatus>)> =
            ran.iter().map(|r| (r.step.as_str(), r.reason)).collect();
        assert_eq!(
            ran,
            [
                ("002_more.sql", Some(ikit_core::ScriptStatus::New)),
                ("R__views.sql", Some(ikit_core::ScriptStatus::Rerun))
            ]
        );
        assert_eq!(
            app.hook_history("wl-acme-dev-001", Some("schema"), 10)
                .await
                .unwrap()
                .len(),
            4
        );
    }

    #[tokio::test]
    async fn changed_scripts_are_refused_unless_allowed() {
        let (_dir, app, _, sql) = setup_postgres().await;
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        app.deploy_apply(&plan, "test", false).await.unwrap();

        std::fs::write(
            sql.path().join("001_init.sql"),
            "CREATE TABLE orders (id bigint);",
        )
        .unwrap();
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        assert_eq!(plan.hooks[0].changed(), ["001_init.sql"]);
        assert!(plan.needs_apply());
        let err = app.deploy_apply(&plan, "test", false).await.unwrap_err();
        assert!(err.to_string().contains("--allow-changed"), "{err}");
        assert_eq!(
            app.workload_get("wl-acme-dev-001").await.unwrap().status,
            WorkloadStatus::Failed
        );

        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        let report = app.deploy_apply(&plan, "test", true).await.unwrap();
        assert_eq!(
            report.hooks_ran[0].reason,
            Some(ikit_core::ScriptStatus::Changed)
        );
    }

    #[tokio::test]
    async fn failing_script_fails_the_apply_and_is_retried() {
        let (_dir, app, docker, sql) = setup_postgres().await;
        std::fs::write(sql.path().join("002_bad.sql"), "FAIL;").unwrap();
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        let err = app.deploy_apply(&plan, "test", false).await.unwrap_err();
        assert!(
            err.to_string().contains("002_bad.sql failed (rolled back)"),
            "{err}"
        );
        assert!(
            docker
                .ledger
                .lock()
                .unwrap()
                .contains_key(&("schema".into(), "001_init.sql".into()))
        );
        let history = app.hook_history("wl-acme-dev-001", None, 10).await.unwrap();
        assert_eq!(history[0].result, RunResult::Failed);

        std::fs::write(sql.path().join("002_bad.sql"), "SELECT 1;").unwrap();
        let plan = app.deploy_plan("wl-acme-dev-001").await.unwrap();
        let report = app.deploy_apply(&plan, "test", false).await.unwrap();
        let ran: Vec<&str> = report.hooks_ran.iter().map(|r| r.step.as_str()).collect();
        // The run stopped at the failure, so the script after it had not run either.
        assert_eq!(
            ran,
            ["002_bad.sql", "R__views.sql"],
            "001_init.sql is not run again"
        );
    }

    #[tokio::test]
    async fn hook_add_validates_and_protects_its_service() {
        let (_dir, app, _, sql) = setup_postgres().await;
        let mut req = sql_hook("seed", "nope", sql.path().to_path_buf());
        assert!(matches!(
            app.hook_add("wl-acme-dev-001", req.clone()).await,
            Err(AppError::NotFound(_))
        ));
        req = sql_hook("schema", "postgres", sql.path().to_path_buf());
        assert!(matches!(
            app.hook_add("wl-acme-dev-001", req.clone()).await,
            Err(AppError::AlreadyExists(_))
        ));
        req = sql_hook("seed", "postgres", sql.path().join("missing.sql"));
        assert!(app.hook_add("wl-acme-dev-001", req).await.is_err());

        let err = app
            .services_remove("wl-acme-dev-001", &["postgres".into()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("hook `schema`"), "{err}");
        app.hook_remove("wl-acme-dev-001", "schema").await.unwrap();
        app.services_remove("wl-acme-dev-001", &["postgres".into()])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn backups_round_trip_through_storage() {
        let (dir, app, docker, _sql) = setup_postgres().await;
        let wl = "wl-acme-dev-001";
        assert!(
            app.backup_run(wl, "postgres", None, None, "test")
                .await
                .is_err(),
            "not deployed yet"
        );
        let plan = app.deploy_plan(wl).await.unwrap();
        app.deploy_apply(&plan, "test", false).await.unwrap();
        let req = crate::NewStorageRequest {
            name: "nas".into(),
            provider: ikit_core::StorageProvider::Local,
            bucket: "local".into(),
            prefix: "backups".into(),
            profile: None,
            options: ikit_core::StorageOptions {
                root: Some(dir.path().join("blobs")),
                ..Default::default()
            },
        };
        app.storage_add(req).await.unwrap();

        let first = app
            .backup_run(wl, "postgres", None, None, "test")
            .await
            .unwrap()
            .backup;
        assert!(
            first
                .object_key
                .starts_with("backups/acme/wl-acme-dev-001/postgres/postgres-"),
            "{}",
            first.object_key
        );
        assert!(first.object_key.ends_with(".pgdump.zst"));
        assert!(
            app.backup_run(wl, "redis-nope", None, None, "test")
                .await
                .is_err()
        );

        app.backup_restore(first.id, "test").await.unwrap();
        let restored = docker
            .sql
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.starts_with("RESTORE"))
            .cloned()
            .unwrap();
        assert!(
            restored.contains("80, 71, 68, 77, 80, 0"),
            "the exact dump bytes are restored: {restored}"
        );

        // Retention keeps the newest.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let report = app
            .backup_run(wl, "postgres", Some("nas"), Some(1), "test")
            .await
            .unwrap();
        assert_eq!(
            report.pruned.iter().map(|b| b.id).collect::<Vec<_>>(),
            [first.id]
        );
        assert_eq!(app.backup_list(Some(wl), None).await.unwrap().len(), 1);
        assert!(
            app.storage_remove("nas").await.is_err(),
            "storage with backups is protected"
        );

        // A tampered object is refused.
        let latest = report.backup;
        std::fs::write(
            dir.path().join("blobs").join(&latest.object_key),
            b"tampered",
        )
        .unwrap();
        let err = app.backup_restore(latest.id, "test").await.unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
    }

    #[tokio::test]
    async fn prod_requires_pinned_images() {
        let (_dir, app, _) = app().await;
        let app = app.with_exec(Arc::new(FakeDocker::default()));
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        app.workload_create("prod", "laptop").await.unwrap();
        let req = ServiceRequest {
            name: None,
            registry: "redis".into(),
            constraint: "7".parse().unwrap(),
            group: Category::Data,
            env: BTreeMap::new(),
        };
        app.services_add("wl-acme-prod-001", vec![req])
            .await
            .unwrap();
        let plan = app.deploy_plan("wl-acme-prod-001").await.unwrap();
        assert!(plan.blockers[0].contains("redis"), "{:?}", plan.blockers);
        assert!(
            app.deploy_apply(&plan, "test", false)
                .await
                .unwrap_err()
                .to_string()
                .contains("pin")
        );

        let changes = crate::VersionChanges {
            digest: Some(Some("sha256:0123".into())),
            ..Default::default()
        };
        app.registry_version_update("redis", &semver::Version::new(7, 4, 1), changes)
            .await
            .unwrap();
        let plan = app.deploy_plan("wl-acme-prod-001").await.unwrap();
        assert!(plan.blockers.is_empty());
        assert!(
            plan.rendered.compose.contains("redis:7.4.1@sha256:0123"),
            "{}",
            plan.rendered.compose
        );
        app.deploy_apply(&plan, "test", false).await.unwrap();
    }

    #[test]
    fn parses_both_ps_formats() {
        let line =
            r#"{"Service":"a","State":"running","Health":"","Status":"Up","Publishers":null}"#;
        assert_eq!(parse_ps(line)[0].service, "a");
        assert_eq!(parse_ps(&format!("[{line}]"))[0].state, "running");
        assert!(parse_ps("").is_empty());
    }

    #[test]
    fn shell_quotes() {
        assert_eq!(sh("a b"), "'a b'");
        assert_eq!(sh("it's"), r"'it'\''s'");
    }
}
