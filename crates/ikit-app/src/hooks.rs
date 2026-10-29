//! Hooks: SQL scripts, shell commands, HTTP calls and MCP tool calls run around a
//! workload's deploys.
//!
//! - `pre-apply` hooks run before the compose files are written (in-service hooks are skipped
//!   on the first deploy, when nothing is running yet).
//! - `post-apply` hooks run once services are up and healthy. SQL hooks only run here.
//! - `pre-destroy` hooks run before containers are removed.
//!
//! SQL scripts run once each, tracked in the target database (see `hooks_sql`). Shell, HTTP
//! and MCP hooks run every time their phase comes around.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use ikit_core::hook::{classify, expand};
use ikit_core::{
    Hook, HookAction, HookKind, HookPhase, HookRunRecord, HttpAction, McpAction, RunResult,
    ScriptState, ScriptStatus, ShellAction, SqlAction, SqlEngine, SqlVia, Workload, WorkloadStatus,
};
use ikit_state::{NewHook, ScriptRun};
use serde::Serialize;

use crate::deploy::{Place, sh};
use crate::hooks_sql::{SqlConn, load_scripts};
use crate::{App, AppError, Result};

const SHELL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Starting the MCP server (`npx` may download it first) and the tool call together.
const MCP_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
pub struct NewHookRequest {
    pub name: String,
    pub phase: HookPhase,
    /// Defaults to true, except for HTTP hooks (a failed notification should not fail a deploy).
    pub fail_on_error: Option<bool>,
    pub action: HookRequest,
}

#[derive(Debug, Clone)]
pub enum HookRequest {
    Sql {
        service: String,
        /// Detected from the service's registry name when `None`.
        engine: Option<SqlEngine>,
        database: Option<String>,
        /// A `.sql` file or a folder of them.
        source: PathBuf,
        via: SqlVia,
    },
    Shell {
        command: String,
        service: Option<String>,
    },
    Http {
        method: String,
        /// Stored in the keychain, since webhook URLs usually carry a token.
        url: String,
        body: Option<String>,
        headers: BTreeMap<String, String>,
    },
    Mcp {
        /// Program and arguments that start the MCP server over stdio.
        command: Vec<String>,
        tool: String,
        /// A JSON object.
        arguments: serde_json::Value,
        /// Env values for the server kept in the keychain, e.g. an API token.
        secret_env: BTreeMap<String, String>,
    },
}

/// Whether a hook keeps something in the keychain (an HTTP URL, or an MCP server's secrets).
fn has_secret(action: &HookAction) -> bool {
    match action {
        HookAction::Http(_) => true,
        HookAction::Mcp(a) => !a.secret_env.is_empty(),
        _ => false,
    }
}

fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The text parts of a tool result, for error messages.
fn tool_text(result: &rmcp::model::CallToolResult) -> String {
    let content = serde_json::to_value(&result.content).unwrap_or_default();
    let text: Vec<&str> = content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("text")?.as_str())
        .collect();
    let text = text.join(" ");
    if text.chars().count() > 500 {
        format!("{}…", text.chars().take(500).collect::<String>())
    } else {
        text
    }
}

/// What a hook would do on the next apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookPlan {
    pub hook: String,
    pub kind: HookKind,
    pub phase: HookPhase,
    /// What it runs, e.g. `sql in postgres via exec <- /repo/sql`.
    pub action: String,
    /// SQL hooks only.
    pub scripts: Vec<ScriptState>,
    /// Why the ledger could not be checked, if it couldn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl HookPlan {
    pub fn pending(&self) -> usize {
        self.scripts
            .iter()
            .filter(|s| s.status.will_run(false))
            .count()
    }

    pub fn changed(&self) -> Vec<&str> {
        self.scripts
            .iter()
            .filter(|s| s.status == ScriptStatus::Changed)
            .map(|s| s.script.as_str())
            .collect()
    }
}

/// One step a hook ran (a SQL script, a shell command or an HTTP call).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookOutcome {
    pub hook: String,
    pub phase: HookPhase,
    /// The script name, or `shell` / `http`.
    pub step: String,
    /// For SQL scripts: why it ran (new, rerun, changed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ScriptStatus>,
    /// A failure that did not fail the run (`fail_on_error = false`), or why it was skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

fn http_display(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("https://")
        .map(|r| ("https", r))
        .or_else(|| url.strip_prefix("http://").map(|r| ("http", r)));
    match rest {
        Some((scheme, r)) if !r.is_empty() => {
            let host = r.split(['/', '?', '#']).next().unwrap_or("");
            // Drop any `user:pass@`.
            let host = host.rsplit('@').next().unwrap_or(host);
            Ok(format!("{scheme}://{host}"))
        }
        _ => Err(AppError::Invalid(format!(
            "invalid URL `{url}` (expected http:// or https://)"
        ))),
    }
}

impl App {
    fn hook_secret_key(&self, workload: &str, hook: &str) -> String {
        format!("org/{}/hook/{workload}/{hook}", self.org.name)
    }

    pub async fn hook_add(&self, workload: &str, req: NewHookRequest) -> Result<Hook> {
        if !ikit_core::is_slug(&req.name) {
            return Err(AppError::Invalid(format!(
                "invalid hook name `{}`: use {}",
                req.name,
                ikit_core::SLUG_RULE
            )));
        }
        let wl = self.workload_get(workload).await?;
        let services = self.services_list(workload).await?;
        let find_service = |name: &str| {
            services
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| AppError::NotFound(format!("service `{name}` in {workload}")))
        };
        let mut url_secret = None;
        let action = match req.action {
            HookRequest::Sql {
                service,
                engine,
                database,
                source,
                via,
            } => {
                if req.phase != HookPhase::PostApply {
                    return Err(AppError::Invalid(
                        "SQL hooks run post-apply only (the database must be up)".to_owned(),
                    ));
                }
                let svc = find_service(&service)?;
                let engine = match engine.or_else(|| SqlEngine::detect(&svc.registry_service)) {
                    Some(e) => e,
                    None => {
                        return Err(AppError::Invalid(format!(
                            "cannot tell which database `{}` ({}) is; pass --engine postgres|mysql",
                            svc.name, svc.registry_service
                        )));
                    }
                };
                if let Some(db) = &database
                    && !db
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(AppError::Invalid(format!("invalid database name `{db}`")));
                }
                let source = std::fs::canonicalize(&source).map_err(|_| {
                    AppError::Invalid(format!("{} does not exist", source.display()))
                })?;
                load_scripts(&req.name, &source)?;
                HookAction::Sql(SqlAction {
                    service: svc.name.clone(),
                    engine,
                    database,
                    source,
                    via,
                })
            }
            HookRequest::Shell { command, service } => {
                if command.trim().is_empty() {
                    return Err(AppError::Invalid("the shell command is empty".to_owned()));
                }
                if let Some(s) = &service {
                    find_service(s)?;
                }
                HookAction::Shell(ShellAction { command, service })
            }
            HookRequest::Http {
                method,
                url,
                body,
                headers,
            } => {
                let method = method.to_ascii_uppercase();
                if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&method.as_str()) {
                    return Err(AppError::Invalid(format!(
                        "unsupported HTTP method `{method}`"
                    )));
                }
                let url_display = http_display(&url)?;
                url_secret = Some(url);
                HookAction::Http(HttpAction {
                    method,
                    url_display,
                    body,
                    headers,
                })
            }
            HookRequest::Mcp {
                command,
                tool,
                arguments,
                secret_env,
            } => {
                if command.first().is_none_or(|p| p.trim().is_empty()) {
                    return Err(AppError::Invalid(
                        "the MCP server command is empty".to_owned(),
                    ));
                }
                if tool.trim().is_empty() {
                    return Err(AppError::Invalid("the MCP tool name is empty".to_owned()));
                }
                if !arguments.is_object() {
                    return Err(AppError::Invalid(
                        "MCP tool arguments must be a JSON object".to_owned(),
                    ));
                }
                if let Some(bad) = secret_env.keys().find(|k| !is_env_name(k)) {
                    return Err(AppError::Invalid(format!("invalid env var name `{bad}`")));
                }
                if !secret_env.is_empty() {
                    url_secret =
                        Some(serde_json::to_string(&secret_env).expect("a map serializes"));
                }
                HookAction::Mcp(McpAction {
                    command,
                    tool,
                    arguments,
                    secret_env: secret_env.into_keys().collect(),
                })
            }
        };
        let fail_on_error = req
            .fail_on_error
            .unwrap_or(!matches!(action, HookAction::Http(_)));
        let new = NewHook {
            name: &req.name,
            phase: req.phase,
            fail_on_error,
            action: &action,
        };
        let hook =
            self.store
                .hooks()
                .insert(wl.id, new)
                .await
                .map_err(|e| match AppError::from(e) {
                    AppError::AlreadyExists(_) => {
                        AppError::AlreadyExists(format!("hook `{}` in {workload}", req.name))
                    }
                    e => e,
                })?;
        if let Some(url) = url_secret
            && let Err(e) = self
                .secrets
                .set(&self.hook_secret_key(workload, &hook.name), &url)
        {
            self.store.hooks().delete(wl.id, &hook.name).await?;
            return Err(e.into());
        }
        Ok(hook)
    }

    pub async fn hook_list(&self, workload: &str) -> Result<Vec<Hook>> {
        let wl = self.workload_get(workload).await?;
        Ok(self.store.hooks().list(wl.id).await?)
    }

    /// Removes the hook from ikit (and an HTTP hook's URL from the keychain). A SQL hook's
    /// ledger rows in the target database are left alone.
    pub async fn hook_remove(&self, workload: &str, name: &str) -> Result<()> {
        let wl = self.workload_get(workload).await?;
        let hook = self
            .store
            .hooks()
            .list(wl.id)
            .await?
            .into_iter()
            .find(|h| h.name == name)
            .ok_or_else(|| AppError::NotFound(format!("hook `{name}` in {workload}")))?;
        self.store.hooks().delete(wl.id, name).await?;
        if has_secret(&hook.action) {
            self.secrets.delete(&self.hook_secret_key(workload, name))?;
        }
        Ok(())
    }

    /// Keychain entries of a workload's hooks, for cleanup when the workload is removed.
    pub(crate) async fn hook_secret_keys(&self, workload: &str) -> Result<Vec<String>> {
        Ok(self
            .hook_list(workload)
            .await?
            .into_iter()
            .filter(|h| has_secret(&h.action))
            .map(|h| self.hook_secret_key(workload, &h.name))
            .collect())
    }

    pub async fn hook_history(
        &self,
        workload: &str,
        hook: Option<&str>,
        limit: i64,
    ) -> Result<Vec<HookRunRecord>> {
        let wl = self.workload_get(workload).await?;
        Ok(self.store.hooks().history(wl.id, hook, limit).await?)
    }

    async fn sql_conn<'a>(
        &'a self,
        place: &'a Place,
        workload: &'a str,
        action: &'a SqlAction,
    ) -> Result<SqlConn<'a>> {
        Ok(match action.via {
            SqlVia::Exec => SqlConn::Exec {
                app: self,
                place,
                workload,
                action,
            },
            SqlVia::Direct => SqlConn::Direct {
                engine: action.engine,
                target: self.direct_target(workload, action).await?,
            },
        })
    }

    /// What each hook would do on the next apply. Read-only.
    pub(crate) async fn hook_plans(
        &self,
        workload: &str,
        status: WorkloadStatus,
        place: &Place,
    ) -> Result<Vec<HookPlan>> {
        let mut plans = Vec::new();
        for hook in self.hook_list(workload).await? {
            let mut plan = HookPlan {
                hook: hook.name.clone(),
                kind: hook.action.kind(),
                phase: hook.phase,
                action: hook.action.describe(),
                scripts: vec![],
                note: None,
            };
            if let HookAction::Sql(action) = &hook.action {
                match load_scripts(&hook.name, &action.source) {
                    Err(e) => plan.note = Some(e.to_string()),
                    Ok(scripts) => {
                        let pairs: Vec<(String, String)> =
                            scripts.into_iter().map(|s| (s.name, s.checksum)).collect();
                        let ledger = if status == WorkloadStatus::Draft {
                            BTreeMap::new()
                        } else {
                            let conn = self.sql_conn(place, workload, action).await;
                            let read = match &conn {
                                Ok(c) => c.read_ledger(&hook.name, false).await,
                                Err(e) => Err(e.to_string()),
                            };
                            read.unwrap_or_else(|e| {
                                plan.note = Some(format!(
                                    "could not read the ledger ({e}); assuming nothing has run"
                                ));
                                BTreeMap::new()
                            })
                        };
                        plan.scripts = classify(&pairs, &ledger);
                    }
                }
            }
            plans.push(plan);
        }
        Ok(plans)
    }

    /// Runs the workload's hooks for `phase` in order (or only the hook named `only`, in any
    /// phase). Stops at the first failure of a hook with `fail_on_error`.
    pub(crate) async fn run_phase(
        &self,
        wl: &Workload,
        place: &Place,
        phase: HookPhase,
        run_id: Option<i64>,
        allow_changed: bool,
        only: Option<&str>,
    ) -> Result<Vec<HookOutcome>> {
        let hooks = self.hook_list(&wl.name).await?;
        if let Some(name) = only
            && !hooks.iter().any(|h| h.name == name)
        {
            return Err(AppError::NotFound(format!("hook `{name}` in {}", wl.name)));
        }
        let selected = hooks.iter().filter(|h| match only {
            Some(name) => h.name == name,
            None => h.phase == phase,
        });
        let target = self.target_show(&wl.target).await?;

        let mut out = Vec::new();
        for hook in selected {
            // A hook run by name reports (and sees) its own phase.
            let phase = hook.phase;
            let vars = BTreeMap::from([
                ("org", self.org.name.clone()),
                ("workload", wl.name.clone()),
                ("env", wl.env.clone()),
                ("target", target.name.clone()),
                ("phase", phase.to_string()),
                ("run_id", run_id.map(|r| r.to_string()).unwrap_or_default()),
            ]);
            // Nothing runs in containers before the first deploy.
            let needs_running_service = hook.action.service().is_some()
                && !matches!(&hook.action, HookAction::Sql(a) if a.via == SqlVia::Direct);
            if needs_running_service
                && wl.status == WorkloadStatus::Draft
                && phase == HookPhase::PreApply
            {
                out.push(HookOutcome {
                    hook: hook.name.clone(),
                    phase,
                    step: hook.action.kind().to_string(),
                    reason: None,
                    warning: Some(
                        "skipped: its service is not running yet (first deploy)".to_owned(),
                    ),
                });
                continue;
            }
            match &hook.action {
                HookAction::Sql(action) => {
                    out.extend(
                        self.run_sql_hook(wl, place, hook, action, phase, run_id, allow_changed)
                            .await?,
                    );
                }
                HookAction::Shell(action) => {
                    let result = self.run_shell(wl, place, action, &vars).await;
                    out.extend(
                        self.finish_step(hook, phase, run_id, "shell", result)
                            .await?,
                    );
                }
                HookAction::Http(action) => {
                    let result = self.run_http(&wl.name, hook, action, &vars).await;
                    out.extend(
                        self.finish_step(hook, phase, run_id, "http", result)
                            .await?,
                    );
                }
                HookAction::Mcp(action) => {
                    let result = self.run_mcp(&wl.name, hook, action, &vars).await;
                    out.extend(self.finish_step(hook, phase, run_id, "mcp", result).await?);
                }
            }
        }
        Ok(out)
    }

    /// Records a shell/HTTP step and applies the hook's failure policy.
    async fn finish_step(
        &self,
        hook: &Hook,
        phase: HookPhase,
        run_id: Option<i64>,
        step: &str,
        result: std::result::Result<(), String>,
    ) -> Result<Option<HookOutcome>> {
        let error = result.err();
        let record = ScriptRun {
            run_id,
            script: step,
            checksum: "",
            result: if error.is_some() {
                RunResult::Failed
            } else {
                RunResult::Ok
            },
            error: error.as_deref(),
        };
        self.store.hooks().record(hook.id, record).await?;
        match error {
            Some(e) if hook.fail_on_error => Err(AppError::Invalid(format!(
                "hook `{}` ({step}) failed: {e}",
                hook.name
            ))),
            warning => Ok(Some(HookOutcome {
                hook: hook.name.clone(),
                phase,
                step: step.to_owned(),
                reason: None,
                warning,
            })),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_sql_hook(
        &self,
        wl: &Workload,
        place: &Place,
        hook: &Hook,
        action: &SqlAction,
        phase: HookPhase,
        run_id: Option<i64>,
        allow_changed: bool,
    ) -> Result<Vec<HookOutcome>> {
        let scripts = load_scripts(&hook.name, &action.source)?;
        let conn = self.sql_conn(place, &wl.name, action).await?;
        let ledger = conn.read_ledger(&hook.name, true).await.map_err(|e| {
            AppError::Invalid(format!(
                "hook `{}`: reading the ledger in `{}` failed: {e}",
                hook.name, action.service
            ))
        })?;
        let pairs: Vec<(String, String)> = scripts
            .iter()
            .map(|s| (s.name.clone(), s.checksum.clone()))
            .collect();
        let states = classify(&pairs, &ledger);
        let changed: Vec<&str> = states
            .iter()
            .filter(|s| s.status == ScriptStatus::Changed)
            .map(|s| s.script.as_str())
            .collect();
        if !changed.is_empty() && !allow_changed {
            return Err(AppError::Invalid(format!(
                "hook `{}`: {} changed after being applied; add a new script instead, or pass --allow-changed to run it again",
                hook.name,
                changed.join(", ")
            )));
        }
        let mut out = Vec::new();
        for (script, state) in scripts.iter().zip(&states) {
            if !state.status.will_run(allow_changed) {
                continue;
            }
            let error = conn.run_script(&hook.name, script).await.err();
            let result = if error.is_none() {
                RunResult::Ok
            } else {
                RunResult::Failed
            };
            let record = ScriptRun {
                run_id,
                script: &script.name,
                checksum: &script.checksum,
                result,
                error: error.as_deref(),
            };
            self.store.hooks().record(hook.id, record).await?;
            if let Some(e) = error {
                let message = format!(
                    "hook `{}`: {} failed (rolled back): {e}",
                    hook.name, script.name
                );
                if hook.fail_on_error {
                    return Err(AppError::Invalid(message));
                }
                out.push(HookOutcome {
                    hook: hook.name.clone(),
                    phase,
                    step: script.name.clone(),
                    reason: Some(state.status),
                    warning: Some(message),
                });
                break;
            }
            out.push(HookOutcome {
                hook: hook.name.clone(),
                phase,
                step: script.name.clone(),
                reason: Some(state.status),
                warning: None,
            });
        }
        Ok(out)
    }

    async fn run_shell(
        &self,
        wl: &Workload,
        place: &Place,
        action: &ShellAction,
        vars: &BTreeMap<&str, String>,
    ) -> std::result::Result<(), String> {
        let env: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (format!("IKIT_{}", k.to_ascii_uppercase()), v.clone()))
            .collect();
        let out = match (&action.service, place) {
            (Some(service), _) => {
                let mut args: Vec<String> = vec!["exec".into(), "-T".into()];
                for (k, v) in &env {
                    args.extend(["-e".to_owned(), format!("{k}={v}")]);
                }
                args.extend([
                    service.clone(),
                    "sh".into(),
                    "-c".into(),
                    action.command.clone(),
                ]);
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                self.compose_with(place, &wl.name, &refs, None, SHELL_TIMEOUT)
                    .await
            }
            (None, Place::Local(dir)) => {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                let mut args: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
                let script = format!(
                    "cd {} && {}",
                    sh(&dir.display().to_string()),
                    action.command
                );
                args.extend(["sh".into(), "-c".into(), script]);
                self.exec
                    .run("env", &args, None, SHELL_TIMEOUT)
                    .await
                    .map_err(AppError::from)
            }
            (None, Place::Ssh { args: ssh, dir }) => {
                let assigns: Vec<String> =
                    env.iter().map(|(k, v)| format!("{k}={}", sh(v))).collect();
                let script = format!(
                    "mkdir -p {d} && cd {d} && {} sh -c {}",
                    assigns.join(" "),
                    sh(&action.command),
                    d = sh(dir)
                );
                let mut all = ssh.clone();
                all.push(script);
                self.exec
                    .run("ssh", &all, None, SHELL_TIMEOUT)
                    .await
                    .map_err(AppError::from)
            }
        }
        .map_err(|e| e.to_string())?;
        if out.success() {
            Ok(())
        } else {
            Err(out.failure_detail())
        }
    }

    async fn run_http(
        &self,
        workload: &str,
        hook: &Hook,
        action: &HttpAction,
        vars: &BTreeMap<&str, String>,
    ) -> std::result::Result<(), String> {
        let url = self
            .secrets
            .get(&self.hook_secret_key(workload, &hook.name))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "the keychain has no URL for hook `{}`; add the hook again",
                    hook.name
                )
            })?;
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        let method =
            reqwest::Method::from_bytes(action.method.as_bytes()).map_err(|e| e.to_string())?;
        let mut req = client.request(method, &url);
        for (k, v) in &action.headers {
            req = req.header(k, expand(v, vars));
        }
        if let Some(body) = &action.body {
            let body = expand(body, vars);
            if body.trim_start().starts_with(['{', '['])
                && !action
                    .headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("content-type"))
            {
                req = req.header("content-type", "application/json");
            }
            req = req.body(body);
        }
        // Errors may include the URL; report only its display form.
        let response = req.send().await.map_err(|e| {
            format!(
                "{} request to {} failed: {}",
                action.method,
                action.url_display,
                e.without_url()
            )
        })?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!(
                "{} {} returned {status}",
                action.method, action.url_display
            ))
        }
    }

    /// Starts the hook's MCP server, calls its tool once and stops the server.
    async fn run_mcp(
        &self,
        workload: &str,
        hook: &Hook,
        action: &McpAction,
        vars: &BTreeMap<&str, String>,
    ) -> std::result::Result<(), String> {
        use rmcp::ServiceExt;
        use rmcp::model::CallToolRequestParams;
        use rmcp::transport::TokioChildProcess;

        let secret_env: BTreeMap<String, String> = if action.secret_env.is_empty() {
            BTreeMap::new()
        } else {
            let json = self
                .secrets
                .get(&self.hook_secret_key(workload, &hook.name))
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    format!(
                        "the keychain has no secrets for hook `{}`; add the hook again",
                        hook.name
                    )
                })?;
            serde_json::from_str(&json).map_err(|e| format!("bad keychain entry: {e}"))?
        };
        let program = &action.command[0];
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&action.command[1..]).envs(&secret_env);
        for (k, v) in vars {
            cmd.env(format!("IKIT_{}", k.to_ascii_uppercase()), v);
        }
        let (transport, _) = TokioChildProcess::builder(cmd)
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start `{program}`: {e}"))?;
        let arguments = match action.expanded_arguments(vars) {
            serde_json::Value::Object(map) => map,
            _ => Default::default(),
        };
        let call = async {
            let client = ()
                .serve(transport)
                .await
                .map_err(|e| format!("MCP handshake with `{program}` failed: {e}"))?;
            let params = CallToolRequestParams::new(action.tool.clone()).with_arguments(arguments);
            let result = client.call_tool(params).await;
            let _ = client.cancel().await;
            let result =
                result.map_err(|e| format!("calling tool `{}` failed: {e}", action.tool))?;
            if result.is_error == Some(true) {
                return Err(format!(
                    "tool `{}` returned an error: {}",
                    action.tool,
                    tool_text(&result)
                ));
            }
            Ok(())
        };
        tokio::time::timeout(MCP_TIMEOUT, call)
            .await
            .map_err(|_| format!("tool `{}` timed out", action.tool))?
    }

    /// Runs hooks now against the deployed workload, without re-deploying: all post-apply
    /// hooks, or just `only`.
    pub async fn hook_run(
        &self,
        workload: &str,
        only: Option<&str>,
        allow_changed: bool,
        actor: &str,
    ) -> Result<Vec<HookOutcome>> {
        let wl = self.workload_get(workload).await?;
        if wl.status != WorkloadStatus::Applied {
            return Err(AppError::Invalid(format!(
                "{workload} is {}; hooks run against a deployed workload (`ikit apply`)",
                wl.status
            )));
        }
        let target = self.target_show(&wl.target).await?;
        let place = self.place(&target, workload)?;
        let holder = format!("{actor} (pid {})", std::process::id());
        self.lock(&wl, &holder).await?;
        let result = async {
            let plan_json =
                serde_json::json!({ "hook": only, "allow_changed": allow_changed }).to_string();
            let run = ikit_state::NewRun {
                action: ikit_core::RunAction::Hook,
                actor,
                plan_json: &plan_json,
                compose_yaml: None,
            };
            let run_id = self.store.workloads().start_run(wl.id, run).await?;
            match self
                .run_phase(
                    &wl,
                    &place,
                    HookPhase::PostApply,
                    Some(run_id),
                    allow_changed,
                    only,
                )
                .await
            {
                Ok(ran) => {
                    self.store
                        .workloads()
                        .finish_run(run_id, RunResult::Ok, None)
                        .await?;
                    Ok(ran)
                }
                Err(e) => {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretStore;
    use crate::testing::app;

    /// A minimal MCP server over stdio: records each tool call's arguments and env, and
    /// reports a tool error when the arguments contain `"fail": true`.
    const FAKE_MCP: &str = r#"
import json, os, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    if msg["method"] == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "1"}}
    elif msg["method"] == "tools/call":
        args = msg["params"].get("arguments", {})
        with open(sys.argv[1], "a") as f:
            f.write(json.dumps({"tool": msg["params"]["name"], "args": args,
                                "token": os.environ.get("TOKEN"), "wl": os.environ.get("IKIT_WORKLOAD")}) + "\n")
        result = {"content": [{"type": "text", "text": "boom" if args.get("fail") else "ok"}],
                  "isError": bool(args.get("fail"))}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
"#;

    #[tokio::test]
    async fn mcp_hooks_call_a_tool_with_expanded_arguments_and_secrets() {
        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_err()
        {
            eprintln!("python3 not found; skipping");
            return;
        }
        let (dir, app, secrets) = app().await;
        let script = dir.path().join("fake_mcp.py");
        let log = dir.path().join("calls.jsonl");
        std::fs::write(&script, FAKE_MCP).unwrap();
        app.target_attach("laptop", ikit_core::Connection::Local)
            .await
            .unwrap();
        let wl = app.workload_create("dev", "laptop").await.unwrap().name;
        let add = |name: &str, arguments: serde_json::Value| NewHookRequest {
            name: name.into(),
            phase: HookPhase::PostApply,
            fail_on_error: None,
            action: HookRequest::Mcp {
                command: vec![
                    "python3".into(),
                    script.display().to_string(),
                    log.display().to_string(),
                ],
                tool: "notify".into(),
                arguments,
                secret_env: [("TOKEN".to_owned(), "s3cret".to_owned())].into(),
            },
        };
        let hook = app
            .hook_add(
                &wl,
                add(
                    "tell",
                    serde_json::json!({"text": "{{workload}} is up", "n": 1}),
                ),
            )
            .await
            .unwrap();
        assert!(hook.fail_on_error);
        let stored = serde_json::to_string(&hook.action).unwrap();
        assert!(
            !stored.contains("s3cret"),
            "secret values stay out of state: {stored}"
        );

        let vars = BTreeMap::from([("workload", wl.clone()), ("phase", "post-apply".into())]);
        let HookAction::Mcp(action) = &hook.action else {
            unreachable!()
        };
        app.run_mcp(&wl, &hook, action, &vars).await.unwrap();
        let call: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&log).unwrap().trim()).unwrap();
        assert_eq!(call["tool"], "notify");
        assert_eq!(call["args"]["text"], format!("{wl} is up"));
        assert_eq!(call["args"]["n"], 1);
        assert_eq!(
            (call["token"].as_str(), call["wl"].as_str()),
            (Some("s3cret"), Some(wl.as_str()))
        );

        let failing = app
            .hook_add(&wl, add("fails", serde_json::json!({"fail": true})))
            .await
            .unwrap();
        let HookAction::Mcp(action) = &failing.action else {
            unreachable!()
        };
        let e = app.run_mcp(&wl, &failing, action, &vars).await.unwrap_err();
        assert!(e.contains("returned an error: boom"), "{e}");

        app.hook_remove(&wl, "tell").await.unwrap();
        assert!(
            secrets
                .get(&app.hook_secret_key(&wl, "tell"))
                .unwrap()
                .is_none()
        );
        let bad = app.hook_add(&wl, add("bad", serde_json::json!([1]))).await;
        assert!(bad.unwrap_err().to_string().contains("JSON object"));
    }

    #[test]
    fn http_urls_are_displayed_without_secrets() {
        assert_eq!(
            http_display("https://hooks.slack.com/services/T0/B0/secret").unwrap(),
            "https://hooks.slack.com"
        );
        assert_eq!(
            http_display("http://user:pw@example.com:8080/x?token=1").unwrap(),
            "http://example.com:8080"
        );
        assert!(http_display("ftp://x").is_err());
        assert!(http_display("https://").is_err());
    }
}
