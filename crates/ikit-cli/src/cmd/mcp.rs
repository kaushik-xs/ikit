use anyhow::{Context, bail};
use clap::{Subcommand, ValueEnum};

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum McpCmd {
    /// Serve the org's tools over stdio (the default, for local MCP clients such as Claude
    /// Code) or streamable HTTP for remote agents.
    Serve {
        /// Serve over stdin/stdout (the default).
        #[arg(long, conflicts_with = "http")]
        stdio: bool,
        /// Serve over HTTP at http://ADDR/mcp, with bearer tokens from `ikit mcp token create`.
        /// ADDR is HOST:PORT where HOST is localhost, a loopback or ZeroTier address of this
        /// machine, `zt` (the first joined ZeroTier network), a ZeroTier network ID or
        /// interface name. Other interfaces are refused.
        #[arg(long, value_name = "ADDR")]
        http: Option<String>,
        /// Only reach these workloads (repeat or comma-separate); other workloads look
        /// missing and tools for the whole org are refused. Over HTTP, tokens can narrow it
        /// further.
        #[arg(long = "workload", value_name = "NAME", value_delimiter = ',')]
        workloads: Vec<String>,
    },
    /// Manage the bearer tokens `ikit mcp serve --http` accepts.
    #[command(subcommand)]
    Token(TokenCmd),
    /// Register `ikit mcp serve` for the active org (or --org) with an MCP client.
    Install {
        client: Client,
        /// Claude Code config scope.
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
        /// Print the command instead of running it.
        #[arg(long)]
        dry_run: bool,
        /// Register a server that only reaches these workloads (see `mcp serve --workload`).
        #[arg(long = "workload", value_name = "NAME", value_delimiter = ',')]
        workloads: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// Create a token; it is printed once and only its hash is kept.
    Create {
        /// A name for who uses it, e.g. the agent or machine.
        #[arg(default_value = "default")]
        name: String,
        /// Only reach these workloads (repeat or comma-separate); default: the whole org.
        #[arg(long = "workload", value_name = "NAME", value_delimiter = ',')]
        workloads: Vec<String>,
    },
    #[command(alias = "ls")]
    List,
    /// Revoke a token; a running server rejects it from the next request.
    #[command(alias = "remove", alias = "revoke")]
    Rm { name: String },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Client {
    Claude,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Local => "local",
        }
    }
}

/// The scope for `--workload` names, after checking each one exists.
async fn scope(app: &ikit_app::App, workloads: &[String]) -> anyhow::Result<ikit_mcp::Scope> {
    if workloads.is_empty() {
        return Ok(ikit_mcp::Scope::org());
    }
    for w in workloads {
        app.workload_get(w).await?;
    }
    Ok(ikit_mcp::Scope::workloads(workloads.iter().cloned()))
}

pub async fn run(ctx: &Ctx, cmd: McpCmd) -> anyhow::Result<()> {
    // The global `--wl` picks a workload to act on; here a scope is meant, and silently
    // ignoring it would hand out more access than asked for. (IKIT_WL is left alone.)
    if ctx.wl.is_some() && std::env::var_os("IKIT_WL").is_none() {
        bail!("`--wl` does not apply to `ikit mcp`; limit access with `--workload NAME`");
    }
    let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
    match cmd {
        McpCmd::Serve {
            stdio: _,
            http: None,
            workloads,
        } => {
            // Fail fast on a bad org or workload instead of on the first tool call.
            ikit_app::org_show(&ctx.paths, &org).await?;
            let scope = scope(&ctx.app().await?, &workloads).await?;
            tracing::info!(org, ?workloads, "serving MCP over stdio");
            ikit_mcp::serve_stdio(ctx.paths.clone(), org, scope, ctx.identity.clone())
                .await
                .map_err(|e| anyhow::anyhow!(e))
        }
        McpCmd::Serve {
            http: Some(spec),
            workloads,
            ..
        } => {
            let app = ctx.app().await?;
            if app.mcp_token_list()?.is_empty() {
                bail!("no MCP tokens yet; create one with `ikit mcp token create` first");
            }
            let scope = scope(&app, &workloads).await?;
            let addr = app.mcp_http_addr(&spec).await?;
            let limited = if workloads.is_empty() {
                String::new()
            } else {
                format!(", limited to {}", workloads.join(", "))
            };
            eprintln!(
                "Serving MCP for org `{org}`{limited} at http://{addr}/mcp (Ctrl-C to stop)."
            );
            ikit_mcp::serve_http(ctx.paths.clone(), org, scope, ctx.identity.clone(), addr)
                .await
                .map_err(|e| anyhow::anyhow!("serving MCP on {addr}: {e}"))
        }
        McpCmd::Token(cmd) => token(ctx, cmd).await,
        McpCmd::Install {
            client: Client::Claude,
            scope: config_scope,
            dry_run,
            workloads,
        } => {
            ikit_app::org_show(&ctx.paths, &org).await?;
            scope(&ctx.app().await?, &workloads).await?;
            let exe = std::env::current_exe().context("locating the ikit binary")?;
            // A scoped server gets its own name, so it can sit next to the org-wide one.
            let server = match workloads.as_slice() {
                [] => format!("ikit-{org}"),
                [one] => format!("ikit-{one}"),
                _ => format!("ikit-{org}-scoped"),
            };
            let mut args = vec![
                "mcp".to_owned(),
                "add".to_owned(),
                "--scope".to_owned(),
                config_scope.as_str().to_owned(),
                server.clone(),
                "--".to_owned(),
                exe.display().to_string(),
                "--home".to_owned(),
                ctx.paths.home.display().to_string(),
                "--org".to_owned(),
                org.clone(),
                "mcp".to_owned(),
                "serve".to_owned(),
            ];
            if !workloads.is_empty() {
                args.extend(["--workload".to_owned(), workloads.join(",")]);
            }
            let shown = format!("claude {}", args.join(" "));
            if dry_run {
                println!("{shown}");
                return Ok(());
            }
            let status = std::process::Command::new("claude").args(&args).status();
            match status {
                Ok(s) if s.success() => {
                    println!(
                        "Registered MCP server `{server}` with Claude Code ({} scope).",
                        config_scope.as_str()
                    );
                    Ok(())
                }
                Ok(s) => bail!("`claude mcp add` exited with {s}"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    bail!("the `claude` CLI is not on PATH; run this yourself:\n  {shown}")
                }
                Err(e) => Err(e).context("running claude"),
            }
        }
    }
}

async fn token(ctx: &Ctx, cmd: TokenCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    match cmd {
        TokenCmd::Create { name, workloads } => {
            let (record, token) = app.mcp_token_create(&name, &workloads).await?;
            if ctx.output == Output::Json {
                return print_json(&serde_json::json!({
                    "name": record.name,
                    "token": token,
                    "workloads": record.workloads,
                }));
            }
            println!("{token}");
            let limited = if record.workloads.is_empty() {
                String::new()
            } else {
                format!(" (limited to {})", record.workloads.join(", "))
            };
            eprintln!(
                "Created MCP token `{name}`{limited}. It is shown only now; send it as `Authorization: Bearer <token>`, e.g.\n  \
                 claude mcp add --transport http ikit-{org} http://<addr>/mcp --header \"Authorization: Bearer <token>\"",
                org = app.org().name
            );
        }
        TokenCmd::List => {
            let tokens = app.mcp_token_list()?;
            if ctx.output == Output::Json {
                return print_json(&tokens);
            }
            if tokens.is_empty() {
                println!("No MCP tokens. Create one with `ikit mcp token create [name]`.");
                return Ok(());
            }
            let mut t = table(&["NAME", "WORKLOADS", "CREATED"]);
            for tok in &tokens {
                t.add_row(vec![
                    tok.name.clone(),
                    if tok.workloads.is_empty() {
                        "all".to_owned()
                    } else {
                        tok.workloads.join(", ")
                    },
                    tok.created_at.format("%Y-%m-%d %H:%M").to_string(),
                ]);
            }
            println!("{t}");
        }
        TokenCmd::Rm { name } => {
            app.mcp_token_remove(&name)?;
            if ctx.output == Output::Table {
                println!("Revoked MCP token `{name}`.");
            }
        }
    }
    Ok(())
}
