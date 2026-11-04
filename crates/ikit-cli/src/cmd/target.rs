use std::path::PathBuf;

use clap::{ArgGroup, Subcommand};
use ikit_app::ZtSsh;
use ikit_core::target::parse_ssh_destination;
use ikit_core::{Connection, Target, TargetStatus};

use crate::output::{Output, print_json, table};
use crate::{Ctx, warn};

#[derive(Debug, Subcommand)]
pub enum TargetCmd {
    /// Register an existing VM or cluster, then test it.
    #[command(subcommand)]
    Attach(AttachCmd),
    /// List targets.
    #[command(alias = "ls")]
    List,
    /// Show a target and its last test results.
    Show { name: String },
    /// Check a target is reachable and ready for workloads.
    Test { name: String },
    /// Reach an SSH target over ZeroTier from now on: join it to the network over its
    /// current connection, authorize it, and switch to its ZeroTier address.
    UseZt {
        name: String,
        /// ikit network name (see `ikit net zt list`).
        network: String,
        #[arg(long)]
        no_test: bool,
    },
    /// Manage the reverse proxy (Caddy or Ferron) on a VM target's host.
    #[command(subcommand)]
    Edge(crate::cmd::edge::EdgeCmd),
    /// Remove an attached target from ikit (the host or cluster is not touched).
    Detach {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AttachCmd {
    /// A VM running Docker: this machine (--local), one reached over SSH, or one reached
    /// over SSH at its ZeroTier address (--zt with --node).
    #[command(group(ArgGroup::new("remote").args(["ssh", "zt"])))]
    Vm {
        name: String,
        /// This machine.
        #[arg(long, conflicts_with_all = ["ssh", "zt"], required_unless_present_any = ["ssh", "zt"])]
        local: bool,
        /// [user@]host[:port]; uses your ssh config and agent.
        #[arg(long, value_name = "[USER@]HOST[:PORT]", conflicts_with = "zt")]
        ssh: Option<String>,
        /// ikit ZeroTier network the VM is on; it is reached at its address there.
        #[arg(long, value_name = "NETWORK", requires = "node")]
        zt: Option<String>,
        /// The VM's ZeroTier node ID (`zerotier-cli info` on the VM); it is authorized,
        /// even before it joins.
        #[arg(long, requires = "zt")]
        node: Option<String>,
        /// SSH user, with --zt.
        #[arg(long, requires = "zt")]
        user: Option<String>,
        /// SSH port, with --zt.
        #[arg(long, requires = "zt")]
        port: Option<u16>,
        /// SSH private key.
        #[arg(long, requires = "remote", value_name = "FILE")]
        identity: Option<PathBuf>,
        /// Skip the connectivity test.
        #[arg(long)]
        no_test: bool,
    },
    /// A Kubernetes cluster from a kubeconfig context.
    K8s {
        name: String,
        #[arg(long)]
        context: String,
        /// kubeconfig path (default: $KUBECONFIG or ~/.kube/config).
        #[arg(long, value_name = "FILE")]
        kubeconfig: Option<PathBuf>,
        #[arg(long)]
        no_test: bool,
    },
}

pub async fn run(ctx: &Ctx, cmd: TargetCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    match cmd {
        TargetCmd::Attach(AttachCmd::Vm {
            name,
            zt: Some(network),
            node: Some(node),
            user,
            port,
            identity,
            no_test,
            ..
        }) => {
            let ssh = ZtSsh {
                network,
                node,
                user,
                port,
                identity_file: identity,
            };
            let mut target = app.target_attach_zt(&name, ssh).await?;
            if !no_test {
                target = app.target_test(&name).await?;
            }
            render(ctx, &target)?;
            if target.status == TargetStatus::Failed {
                warn(format!(
                    "target {name} was attached but failed its checks; once the VM has joined the \
                     network, run `ikit target test {name}`"
                ));
            }
        }
        TargetCmd::Attach(attach) => {
            let (name, connection, no_test) = match attach {
                AttachCmd::Vm {
                    name,
                    ssh: Some(dest),
                    identity,
                    no_test,
                    ..
                } => {
                    let (user, host, port) =
                        parse_ssh_destination(&dest).map_err(anyhow::Error::msg)?;
                    (
                        name,
                        Connection::Ssh {
                            host,
                            user,
                            port,
                            identity_file: identity,
                            zerotier: None,
                        },
                        no_test,
                    )
                }
                AttachCmd::Vm { name, no_test, .. } => (name, Connection::Local, no_test),
                AttachCmd::K8s {
                    name,
                    context,
                    kubeconfig,
                    no_test,
                } => (
                    name,
                    Connection::K8s {
                        context,
                        kubeconfig,
                    },
                    no_test,
                ),
            };
            let mut target = app.target_attach(&name, connection).await?;
            if !no_test {
                target = app.target_test(&name).await?;
            }
            render(ctx, &target)?;
            if target.status == TargetStatus::Failed {
                warn(format!(
                    "target {name} was attached but failed its checks; fix them and run `ikit target test {name}`"
                ));
            }
        }
        TargetCmd::List => {
            let targets = app.target_list().await?;
            if ctx.output == Output::Json {
                return print_json(&targets);
            }
            if targets.is_empty() {
                println!("No targets. Attach one with `ikit target attach vm <name> --local`.");
                return Ok(());
            }
            let mut t = table(&["NAME", "KIND", "ORIGIN", "CONNECTION", "STATUS", "CHECKED"]);
            for target in &targets {
                t.add_row(vec![
                    target.name.clone(),
                    target.kind.to_string(),
                    target.origin.to_string(),
                    target.connection.describe(),
                    target.status.to_string(),
                    target.last_checked_at.map_or("never".to_owned(), |t| {
                        t.format("%Y-%m-%d %H:%M").to_string()
                    }),
                ]);
            }
            println!("{t}");
        }
        TargetCmd::Show { name } => render(ctx, &app.target_show(&name).await?)?,
        TargetCmd::Test { name } => {
            let target = app.target_test(&name).await?;
            render(ctx, &target)?;
            if target.status == TargetStatus::Failed {
                std::process::exit(1);
            }
        }
        TargetCmd::UseZt {
            name,
            network,
            no_test,
        } => {
            let mut target = app.target_use_zt(&name, &network).await?;
            if !no_test {
                target = app.target_test(&name).await?;
            }
            render(ctx, &target)?;
            if target.status == TargetStatus::Failed {
                warn(format!(
                    "target {name} now uses its ZeroTier address but failed its checks; \
                     run `ikit target test {name}` once the node is online"
                ));
            } else if ctx.output == Output::Table {
                println!(
                    "Target {name} is reached over ZeroTier; its public SSH port can be closed."
                );
            }
        }
        TargetCmd::Edge(cmd) => crate::cmd::edge::run(ctx, &app, cmd).await?,
        TargetCmd::Detach { name, yes } => {
            if ctx.confirm(&format!("Detach target {name} from ikit?"), yes)? {
                app.target_detach(&name).await?;
                if ctx.output == Output::Table {
                    println!("Detached {name}.");
                }
            }
        }
    }
    Ok(())
}

fn render(ctx: &Ctx, target: &Target) -> anyhow::Result<()> {
    if ctx.output == Output::Json {
        return print_json(target);
    }
    let mut t = table(&[]);
    t.add_row(vec!["name".to_owned(), target.name.clone()]);
    t.add_row(vec![
        "kind".to_owned(),
        format!("{} ({})", target.kind, target.origin),
    ]);
    t.add_row(vec!["connection".to_owned(), target.connection.describe()]);
    t.add_row(vec!["status".to_owned(), target.status.to_string()]);
    if let Some(e) = &target.edge {
        t.add_row(vec![
            "reverse proxy".to_owned(),
            format!(
                "{}{}",
                e.kind,
                e.version
                    .as_deref()
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default()
            ),
        ]);
    }
    println!("{t}");
    if !target.last_checks.is_empty() {
        let mut t = table(&["CHECK", "RESULT", "DETAIL"]);
        for c in &target.last_checks {
            t.add_row(vec![
                c.name.clone(),
                if c.ok { "ok" } else { "FAIL" }.to_owned(),
                c.detail.clone(),
            ]);
        }
        println!("{t}");
    }
    Ok(())
}
