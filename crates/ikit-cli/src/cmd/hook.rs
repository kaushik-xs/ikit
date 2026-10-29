use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::bail;
use clap::{Subcommand, ValueEnum};
use ikit_app::{HookRequest, NewHookRequest};
use ikit_core::{HookPhase, SqlEngine, SqlVia};

use crate::Ctx;
use crate::output::{Output, print_json, table};

// Parsed once per process, so the size of `Add` does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum HookCmd {
    /// Add a hook. Exactly one of --source (SQL scripts), --shell, --http or --mcp picks the
    /// kind.
    ///
    /// SQL scripts run once each (tracked in the database's `_ikit_hook_ledger`); `R__*.sql`
    /// scripts re-run when they change. Shell, HTTP and MCP hooks run every time their phase
    /// comes.
    Add {
        name: String,
        /// SQL: a .sql file, or a folder whose *.sql files run in name order.
        #[arg(long, value_name = "PATH", group = "kind")]
        source: Option<PathBuf>,
        /// Shell: a command run with `sh -c`, in --service's container or on the target host.
        /// $IKIT_ORG, $IKIT_WORKLOAD, $IKIT_ENV, $IKIT_TARGET, $IKIT_PHASE, $IKIT_RUN_ID are set.
        #[arg(long, value_name = "COMMAND", group = "kind")]
        shell: Option<String>,
        /// HTTP: `METHOD URL`, e.g. "POST https://hooks.slack.com/services/...". The URL is
        /// kept in the keychain.
        #[arg(long, value_name = "METHOD URL", group = "kind")]
        http: Option<String>,
        /// MCP: the command that starts an MCP server over stdio, split on spaces, e.g.
        /// "npx -y @acme/mcp-server". Runs on this machine; needs --tool.
        #[arg(long, value_name = "COMMAND", group = "kind", requires = "tool")]
        mcp: Option<String>,
        /// MCP: the tool to call.
        #[arg(long)]
        tool: Option<String>,
        /// MCP: tool arguments as a JSON object; {{workload}} and friends are replaced in
        /// string values.
        #[arg(long, value_name = "JSON", default_value = "{}")]
        args: String,
        /// MCP: env var the server needs, kept in the keychain (repeatable). Its value is read
        /// from the same variable in this shell, or prompted for.
        #[arg(long = "secret-env", value_name = "NAME")]
        secret_env: Vec<String>,
        /// Service the hook runs in (required for SQL).
        #[arg(long)]
        service: Option<String>,
        /// When it runs (SQL hooks run post-apply only).
        #[arg(long, value_enum, default_value_t = Phase::PostApply)]
        phase: Phase,
        /// SQL: `exec` (through the container, no ports needed) or `direct` (to the service's
        /// published port).
        #[arg(long, default_value = "exec")]
        via: SqlVia,
        /// SQL: postgres or mysql (detected from the service when omitted).
        #[arg(long)]
        engine: Option<SqlEngine>,
        /// SQL: database to run in (default: the one the service's env configures).
        #[arg(long)]
        database: Option<String>,
        /// HTTP: request body; {{org}}, {{workload}}, {{env}}, {{target}}, {{phase}} and
        /// {{run_id}} are replaced.
        #[arg(long)]
        body: Option<String>,
        /// HTTP: header `Name: value` (repeatable).
        #[arg(long = "header", value_name = "NAME: VALUE")]
        headers: Vec<String>,
        /// Report failures as warnings instead of failing the run (the default for HTTP).
        #[arg(long, conflicts_with = "fail_on_error")]
        warn_only: bool,
        /// Fail the run when this hook fails (the default for SQL and shell).
        #[arg(long)]
        fail_on_error: bool,
    },
    /// List the workload's hooks.
    #[command(alias = "ls")]
    List,
    /// Remove a hook (a SQL hook's ledger rows stay in the database).
    #[command(alias = "remove")]
    Rm {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// Run post-apply hooks now (or just one hook) against the deployed workload, without
    /// re-deploying.
    Run {
        /// Only this hook.
        name: Option<String>,
        /// Also re-run SQL scripts that changed after they were applied.
        #[arg(long)]
        allow_changed: bool,
    },
    /// Show hook steps that ran (or failed), newest first.
    History {
        name: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Phase {
    PreApply,
    PostApply,
    PreDestroy,
}

impl From<Phase> for HookPhase {
    fn from(p: Phase) -> Self {
        match p {
            Phase::PreApply => HookPhase::PreApply,
            Phase::PostApply => HookPhase::PostApply,
            Phase::PreDestroy => HookPhase::PreDestroy,
        }
    }
}

pub async fn run(ctx: &Ctx, cmd: HookCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    let wl = app.resolve_workload_name(ctx.wl.as_deref())?;
    match cmd {
        HookCmd::Add {
            name,
            source,
            shell,
            http,
            mcp,
            tool,
            args,
            secret_env,
            service,
            phase,
            via,
            engine,
            database,
            body,
            headers,
            warn_only,
            fail_on_error,
        } => {
            let action = if let Some(source) = source {
                let service = service
                    .ok_or_else(|| anyhow::anyhow!("--service is required for SQL hooks"))?;
                HookRequest::Sql {
                    service,
                    engine,
                    database,
                    source,
                    via,
                }
            } else if let Some(command) = shell {
                HookRequest::Shell { command, service }
            } else if let Some(spec) = http {
                let (method, url) =
                    spec.trim().split_once(char::is_whitespace).ok_or_else(|| {
                        anyhow::anyhow!(
                            "--http takes `METHOD URL`, e.g. \"POST https://example.com/hook\""
                        )
                    })?;
                let mut parsed = BTreeMap::new();
                for h in &headers {
                    let (k, v) = h
                        .split_once(':')
                        .ok_or_else(|| anyhow::anyhow!("--header takes `Name: value`"))?;
                    parsed.insert(k.trim().to_owned(), v.trim().to_owned());
                }
                HookRequest::Http {
                    method: method.to_owned(),
                    url: url.trim().to_owned(),
                    body,
                    headers: parsed,
                }
            } else if let Some(command) = mcp {
                let arguments: serde_json::Value = serde_json::from_str(&args)
                    .map_err(|e| anyhow::anyhow!("--args is not valid JSON: {e}"))?;
                let mut secrets = BTreeMap::new();
                for name in secret_env {
                    let value = match std::env::var(&name) {
                        Ok(v) => v,
                        Err(_) if ctx.interactive => {
                            inquire::Password::new(&format!("Value for {name}:"))
                                .without_confirmation()
                                .prompt()?
                        }
                        Err(_) => bail!("set {name} in the environment (no terminal to prompt)"),
                    };
                    secrets.insert(name, value);
                }
                HookRequest::Mcp {
                    command: command.split_whitespace().map(str::to_owned).collect(),
                    tool: tool.unwrap_or_default(),
                    arguments,
                    secret_env: secrets,
                }
            } else {
                bail!("pass one of --source (SQL), --shell, --http or --mcp");
            };
            let fail_on_error = if warn_only {
                Some(false)
            } else if fail_on_error {
                Some(true)
            } else {
                None
            };
            let req = NewHookRequest {
                name,
                phase: phase.into(),
                fail_on_error,
                action,
            };
            let hook = app.hook_add(&wl, req).await?;
            if ctx.output == Output::Json {
                return print_json(&hook);
            }
            let policy = if hook.fail_on_error {
                "fails the run on error"
            } else {
                "warns on error"
            };
            println!(
                "Added hook `{}`: {} at {}, {policy}.",
                hook.name,
                hook.action.describe(),
                hook.phase
            );
        }
        HookCmd::List => {
            let hooks = app.hook_list(&wl).await?;
            if ctx.output == Output::Json {
                return print_json(&hooks);
            }
            if hooks.is_empty() {
                println!(
                    "{wl} has no hooks. Add one with `ikit hook add <name> --source|--shell|--http ...`."
                );
                return Ok(());
            }
            let mut t = table(&["#", "HOOK", "WHEN", "KIND", "ON ERROR", "ACTION"]);
            for h in &hooks {
                t.add_row(vec![
                    h.ord.to_string(),
                    h.name.clone(),
                    h.phase.to_string(),
                    h.action.kind().to_string(),
                    if h.fail_on_error { "fail" } else { "warn" }.to_owned(),
                    h.action.describe(),
                ]);
            }
            println!("{t}");
        }
        HookCmd::Rm { name, yes } => {
            if ctx.confirm(&format!("Remove hook {name} from {wl}?"), yes)? {
                app.hook_remove(&wl, &name).await?;
                if ctx.output == Output::Table {
                    println!("Removed hook {name}.");
                }
            }
        }
        HookCmd::Run {
            name,
            allow_changed,
        } => {
            let ran = app
                .hook_run(&wl, name.as_deref(), allow_changed, &super::deploy::actor())
                .await?;
            if ctx.output == Output::Json {
                return print_json(&ran);
            }
            if ran.is_empty() {
                println!("Nothing to run.");
            }
            super::deploy::print_outcomes(&ran);
        }
        HookCmd::History { name, limit } => {
            let history = app.hook_history(&wl, name.as_deref(), limit).await?;
            if ctx.output == Output::Json {
                return print_json(&history);
            }
            if history.is_empty() {
                println!("No hooks have run yet.");
                return Ok(());
            }
            let mut t = table(&["HOOK", "STEP", "RESULT", "RAN", "CHECKSUM", "ERROR"]);
            for r in &history {
                t.add_row(vec![
                    r.hook.clone(),
                    r.script.clone(),
                    r.result.to_string(),
                    r.ran_at.format("%Y-%m-%d %H:%M:%S").to_string(),
                    r.checksum[..12.min(r.checksum.len())].to_owned(),
                    r.error.clone().unwrap_or_default(),
                ]);
            }
            println!("{t}");
        }
    }
    Ok(())
}
