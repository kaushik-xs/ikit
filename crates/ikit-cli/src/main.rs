use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod access;
mod cmd;
mod output;

use access::Access;
use ikit_app::SyncOutcome;
use output::Output;

#[derive(Debug, Parser)]
#[command(
    name = "ikit",
    version,
    about = "Create, update, destroy and manage infrastructure components"
)]
struct Cli {
    /// ikit home directory holding the orgs and context.yaml.
    #[arg(long, global = true, env = "IKIT_HOME", value_name = "DIR")]
    home: Option<PathBuf>,

    /// Org to act on (default: the one selected with `ikit org use`).
    #[arg(long, global = true, env = "IKIT_ORG", value_name = "NAME")]
    org: Option<String>,

    /// Workload to act on (default: the one selected with `ikit wl use`).
    #[arg(long, global = true, env = "IKIT_WL", value_name = "NAME")]
    wl: Option<String>,

    /// Output format.
    #[arg(long, short, global = true, value_enum, default_value_t = Output::Table)]
    output: Output,

    /// Never prompt; fail instead when input is missing.
    #[arg(long, global = true)]
    no_input: bool,

    /// Private key for orgs with encrypted secrets: an age key file, or an unencrypted SSH
    /// key with its .pub beside it (default: IKIT_AGE_IDENTITY, then the keychain).
    #[arg(long, global = true, env = "IKIT_IDENTITY_FILE", value_name = "FILE")]
    identity_file: Option<PathBuf>,

    /// Seconds to wait for another writer to release an org's remote lock.
    #[arg(
        long,
        global = true,
        env = "IKIT_LOCK_TIMEOUT",
        value_name = "SECS",
        default_value_t = 60
    )]
    lock_timeout: u64,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace).
    #[arg(long, short, global = true, action = ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

// Parsed once per run, so variant size does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
enum Command {
    /// Create the ikit home and migrate every org's state database (safe to re-run).
    Init,
    /// Manage orgs: each has its own state database, registry, targets and workloads.
    #[command(subcommand)]
    Org(cmd::org::OrgCmd),
    /// This machine's key for reading orgs' encrypted secrets.
    #[command(subcommand)]
    Identity(cmd::identity::IdentityCmd),
    /// Manage provider profiles (aws, azure, gcp, zerotier, k8s).
    #[command(subcommand)]
    Profile(cmd::profile::ProfileCmd),
    /// Manage the service registry: services, versions and images.
    #[command(subcommand)]
    Registry(cmd::registry::RegistryCmd),
    /// Manage targets: the VMs and Kubernetes clusters workloads run on.
    #[command(subcommand)]
    Target(cmd::target::TargetCmd),
    /// Manage workloads (<prefix>-<env>-<nnn>, e.g. wl-acme-dev-001).
    #[command(subcommand, name = "wl", alias = "workload")]
    Workload(cmd::workload::WorkloadCmd),
    /// Manage a workload's service groups (app, platform, data).
    #[command(subcommand, name = "sg", alias = "group")]
    Group(cmd::group::GroupCmd),
    /// Show the compose file the selected workload deploys (secret values are not shown).
    Render,
    /// Show what `apply` would change on the workload's target.
    Plan,
    /// Deploy the selected workload to its target with Docker Compose.
    Apply {
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
        /// Re-run hook scripts that changed after they were applied.
        #[arg(long)]
        allow_changed: bool,
        /// Apply even when the plan has no changes (recreates drifted containers).
        #[arg(long)]
        force: bool,
    },
    /// Show the workload's containers on its target.
    Status {
        /// Compare running containers with the applied files; exits 2 when they differ.
        #[arg(long)]
        drift: bool,
        /// With --drift: every applied workload in the org.
        #[arg(long, requires = "drift")]
        all: bool,
    },
    /// Stop and remove the workload's containers; the definition is kept.
    Destroy {
        /// Also delete its volumes (all data).
        #[arg(long)]
        volumes: bool,
        #[arg(long, short)]
        yes: bool,
    },
    /// Manage blob storage (S3/MinIO, Azure, GCS, local) for backups.
    #[command(subcommand)]
    Storage(cmd::storage::StorageCmd),
    /// Back up and restore database services.
    #[command(subcommand)]
    Backup(cmd::backup::BackupCmd),
    /// Manage SQL hooks: scripts run in a database service after each apply.
    #[command(subcommand)]
    Hook(cmd::hook::HookCmd),
    /// Serve ikit tools to AI agents over MCP (stdio or HTTP).
    #[command(subcommand)]
    Mcp(cmd::mcp::McpCmd),
    /// Manage networks: ZeroTier networks, members and links to workloads.
    #[command(subcommand)]
    Net(cmd::net::NetCmd),
    /// Route domains to the workload's services through the target's reverse proxy.
    #[command(subcommand)]
    Route(cmd::route::RouteCmd),
    /// Buckets and queues services need, and the backends (AWS, or S3/SQS-compatible servers)
    /// that provide them.
    #[command(subcommand, name = "res", alias = "resource")]
    Resource(cmd::resource::ResourceCmd),
    /// Manage individual services in a workload.
    #[command(subcommand, name = "svc", alias = "service")]
    Service(cmd::service::ServiceCmd),
}

impl Ctx {
    pub async fn app(&self) -> anyhow::Result<ikit_app::App> {
        let org = ikit_app::resolve_org(&self.paths, self.org.as_deref())?;
        Ok(ikit_app::App::open(&self.paths, &org, self.secrets(&org)?).await?)
    }

    /// The secret store `org` uses: its encrypted file, or the keychain.
    pub fn secrets(&self, org: &str) -> anyhow::Result<std::sync::Arc<dyn ikit_app::SecretStore>> {
        Ok(ikit_app::org_secret_store(
            &self.paths,
            org,
            &self.identity,
        )?)
    }

    /// Asks for confirmation unless `yes`; fails when prompts are disabled.
    pub fn confirm(&self, question: &str, yes: bool) -> anyhow::Result<bool> {
        if yes {
            return Ok(true);
        }
        if !self.interactive {
            anyhow::bail!("{question} Pass --yes to confirm without a prompt.");
        }
        Ok(inquire::Confirm::new(question)
            .with_default(false)
            .prompt()?)
    }
}

/// Prints a warning to stderr, so JSON on stdout stays clean.
pub fn warn(message: impl std::fmt::Display) {
    eprintln!("warning: {message}");
}

/// Settings every command needs.
#[derive(Clone)]
pub struct Ctx {
    pub paths: ikit_app::Paths,
    pub org: Option<String>,
    pub wl: Option<String>,
    pub output: Output,
    pub interactive: bool,
    pub identity: ikit_app::IdentityOptions,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Exit quietly when stdout is closed early (e.g. `ikit registry export | head`)
    // instead of panicking on a broken pipe.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let ctx = Ctx {
        paths: ikit_app::Paths::resolve(cli.home)?,
        org: cli.org,
        wl: cli.wl,
        output: cli.output,
        interactive: !cli.no_input && std::io::stdin().is_terminal(),
        identity: ikit_app::IdentityOptions {
            file: cli.identity_file,
            ..Default::default()
        },
    };

    let access = cli.command.access();
    let org = match access {
        Access::None => None,
        _ => ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref()).ok(),
    };
    let session = match (&org, access) {
        (Some(org), Access::Read | Access::Write) => {
            let opts = ikit_app::SessionOptions {
                command: command_line(),
                lock_timeout: Duration::from_secs(cli.lock_timeout),
            };
            ikit_app::Session::begin(&ctx.paths, org, access == Access::Write, &opts).await?
        }
        _ => None,
    };
    let Some(session) = session else {
        return dispatch(&ctx, cli.command).await;
    };
    for notice in session.notices() {
        warn(notice);
    }

    // Ctrl-C still pushes what the command changed so far and releases the lock.
    let result = tokio::select! {
        r = dispatch(&ctx, cli.command) => r,
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("interrupted")),
    };
    match session.finish().await {
        Ok(SyncOutcome::Pushed { generation }) => {
            eprintln!("remote: pushed generation {generation}")
        }
        Ok(SyncOutcome::Unpushed) => warn(
            "this command changed local state but does not take the remote lock; \
             it will be pushed by the next change (see `ikit org status`)",
        ),
        Ok(SyncOutcome::Unchanged) => {}
        Err(e) if result.is_ok() => return Err(e.into()),
        Err(e) => warn(format!("remote: {e}")),
    }
    result
}

/// The command line as typed, recorded in the remote lock and HEAD.
pub fn command_line() -> String {
    let args: Vec<String> = std::env::args().skip(1).collect();
    format!("ikit {}", args.join(" "))
}

async fn dispatch(ctx: &Ctx, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Init => cmd::init::run(ctx).await,
        Command::Org(c) => cmd::org::run(ctx, c).await,
        Command::Identity(c) => cmd::identity::run(ctx, c).await,
        Command::Profile(c) => cmd::profile::run(ctx, c).await,
        Command::Registry(c) => cmd::registry::run(ctx, c).await,
        Command::Target(c) => cmd::target::run(ctx, c).await,
        Command::Workload(c) => cmd::workload::run(ctx, c).await,
        Command::Group(c) => cmd::group::run(ctx, c).await,
        Command::Service(c) => cmd::service::run(ctx, c).await,
        Command::Hook(c) => cmd::hook::run(ctx, c).await,
        Command::Storage(c) => cmd::storage::run(ctx, c).await,
        Command::Backup(c) => cmd::backup::run(ctx, c).await,
        Command::Mcp(c) => cmd::mcp::run(ctx, c).await,
        Command::Net(c) => cmd::net::run(ctx, c).await,
        Command::Route(c) => cmd::route::run(ctx, c).await,
        Command::Resource(c) => cmd::resource::run(ctx, c).await,
        Command::Render => cmd::deploy::render(ctx).await,
        Command::Plan => cmd::deploy::plan(ctx).await,
        Command::Apply {
            yes,
            allow_changed,
            force,
        } => cmd::deploy::apply(ctx, yes, allow_changed, force).await,
        Command::Status { drift: false, .. } => cmd::deploy::status(ctx).await,
        Command::Status { drift: true, all } => cmd::deploy::drift(ctx, all).await,
        Command::Destroy { volumes, yes } => cmd::deploy::destroy(ctx, volumes, yes).await,
    }
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("IKIT_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .init();
}
