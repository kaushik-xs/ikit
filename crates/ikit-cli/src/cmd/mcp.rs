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
    },
}

#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// Create a token; it is printed once and only its hash is kept.
    Create {
        /// A name for who uses it, e.g. the agent or machine.
        #[arg(default_value = "default")]
        name: String,
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

pub async fn run(ctx: &Ctx, cmd: McpCmd) -> anyhow::Result<()> {
    let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
    match cmd {
        McpCmd::Serve {
            stdio: _,
            http: None,
        } => {
            // Fail fast on a bad org instead of on the first tool call.
            ikit_app::org_show(&ctx.paths, &org).await?;
            tracing::info!(org, "serving MCP over stdio");
            ikit_mcp::serve_stdio(ctx.paths.clone(), org)
                .await
                .map_err(|e| anyhow::anyhow!(e))
        }
        McpCmd::Serve {
            http: Some(spec), ..
        } => {
            let app = ctx.app().await?;
            if app.mcp_token_list()?.is_empty() {
                bail!("no MCP tokens yet; create one with `ikit mcp token create` first");
            }
            let addr = app.mcp_http_addr(&spec).await?;
            eprintln!("Serving MCP for org `{org}` at http://{addr}/mcp (Ctrl-C to stop).");
            ikit_mcp::serve_http(ctx.paths.clone(), org, addr)
                .await
                .map_err(|e| anyhow::anyhow!("serving MCP on {addr}: {e}"))
        }
        McpCmd::Token(cmd) => token(ctx, cmd).await,
        McpCmd::Install {
            client: Client::Claude,
            scope,
            dry_run,
        } => {
            ikit_app::org_show(&ctx.paths, &org).await?;
            let exe = std::env::current_exe().context("locating the ikit binary")?;
            let args = vec![
                "mcp".to_owned(),
                "add".to_owned(),
                "--scope".to_owned(),
                scope.as_str().to_owned(),
                format!("ikit-{org}"),
                "--".to_owned(),
                exe.display().to_string(),
                "--home".to_owned(),
                ctx.paths.home.display().to_string(),
                "--org".to_owned(),
                org.clone(),
                "mcp".to_owned(),
                "serve".to_owned(),
            ];
            let shown = format!("claude {}", args.join(" "));
            if dry_run {
                println!("{shown}");
                return Ok(());
            }
            let status = std::process::Command::new("claude").args(&args).status();
            match status {
                Ok(s) if s.success() => {
                    println!(
                        "Registered MCP server `ikit-{org}` with Claude Code ({} scope).",
                        scope.as_str()
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
        TokenCmd::Create { name } => {
            let (record, token) = app.mcp_token_create(&name)?;
            if ctx.output == Output::Json {
                return print_json(&serde_json::json!({ "name": record.name, "token": token }));
            }
            println!("{token}");
            eprintln!(
                "Created MCP token `{name}`. It is shown only now; send it as `Authorization: Bearer <token>`, e.g.\n  \
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
            let mut t = table(&["NAME", "CREATED"]);
            for tok in &tokens {
                t.add_row(vec![
                    tok.name.clone(),
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
