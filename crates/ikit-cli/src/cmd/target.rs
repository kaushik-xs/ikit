use std::path::PathBuf;

use clap::Subcommand;
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
    /// Remove an attached target from ikit (the host or cluster is not touched).
    Detach {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AttachCmd {
    /// A VM running Docker: this machine (--local) or one reached over SSH.
    Vm {
        name: String,
        /// This machine.
        #[arg(long, conflicts_with = "ssh", required_unless_present = "ssh")]
        local: bool,
        /// [user@]host[:port]; uses your ssh config and agent.
        #[arg(long, value_name = "[USER@]HOST[:PORT]")]
        ssh: Option<String>,
        /// SSH private key.
        #[arg(long, requires = "ssh", value_name = "FILE")]
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
