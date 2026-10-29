use clap::Subcommand;
use ikit_core::Network;

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum NetCmd {
    /// ZeroTier networks: create, join, authorize members, link to workloads.
    #[command(subcommand)]
    Zt(ZtCmd),
}

#[derive(Debug, Subcommand)]
pub enum ZtCmd {
    /// Create a private network in ZeroTier Central (needs a zerotier profile).
    Create {
        name: String,
        /// zerotier profile with the Central API token.
        #[arg(long)]
        profile: String,
        /// Managed route and address pool (default: a free 10.147.x.0/24).
        #[arg(long)]
        cidr: Option<String>,
        /// Link a workload to it (default: the selected workload, if any; --no-link to skip).
        #[arg(long)]
        link: Option<String>,
        /// Do not link the selected workload.
        #[arg(long, conflicts_with = "link")]
        no_link: bool,
    },
    /// Register an existing network by its ID.
    Attach {
        name: String,
        /// 16-digit network ID.
        network_id: String,
        #[arg(long)]
        profile: String,
        /// Link a workload to it.
        #[arg(long)]
        link: Option<String>,
    },
    #[command(alias = "ls")]
    List,
    /// Show a network's Central config and members.
    Show { name: String },
    /// List a network's members.
    Members { name: String },
    /// Join this machine (or --target) to the network.
    Join {
        name: String,
        /// VM target to join instead of this machine (over SSH).
        #[arg(long)]
        target: Option<String>,
        /// Also authorize the node in Central.
        #[arg(long)]
        authorize: bool,
        /// Member name in Central (default: the target's name).
        #[arg(long, requires = "authorize")]
        member_name: Option<String>,
    },
    /// Leave the network on this machine (or --target).
    Leave {
        name: String,
        #[arg(long)]
        target: Option<String>,
        /// Also deauthorize the node in Central.
        #[arg(long)]
        deauthorize: bool,
    },
    /// Authorize a node (10-digit ID from `zerotier-cli info`); works before it joins.
    Authorize {
        name: String,
        node: String,
        /// Member name in Central.
        #[arg(long)]
        member_name: Option<String>,
    },
    /// Revoke a node's access.
    Deauthorize { name: String, node: String },
    /// Link the selected workload (or --wl) to the network.
    Link { name: String },
    /// Unlink the selected workload (or --wl) from its network.
    Unlink,
    /// Show this machine's (or --target's) ZeroTier node and joined networks.
    Status {
        #[arg(long)]
        target: Option<String>,
    },
    /// Remove a network from ikit (refused while workloads are linked).
    #[command(alias = "remove")]
    Rm {
        name: String,
        /// Also delete it in ZeroTier Central (networks ikit created only).
        #[arg(long)]
        delete: bool,
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(ctx: &Ctx, cmd: NetCmd) -> anyhow::Result<()> {
    let NetCmd::Zt(cmd) = cmd;
    let app = ctx.app().await?;
    let selected_wl = || app.resolve_workload_name(ctx.wl.as_deref());
    match cmd {
        ZtCmd::Create {
            name,
            profile,
            cidr,
            link,
            no_link,
        } => {
            let cidr = cidr.map(|c| c.parse()).transpose()?;
            let link = match (link, no_link) {
                (Some(w), _) => Some(w),
                (None, false) => selected_wl().ok(),
                (None, true) => None,
            };
            let n = app
                .zt_create(&name, &profile, cidr, link.as_deref())
                .await?;
            if ctx.output == Output::Json {
                return print_json(&n);
            }
            println!(
                "Created ZeroTier network `{}` ({}, {}){}.",
                n.name,
                n.network_id,
                n.cidr.as_deref().unwrap_or_default(),
                linked(&n)
            );
            println!(
                "Join a machine with `ikit net zt join {} [--target <vm>] --authorize`.",
                n.name
            );
        }
        ZtCmd::Attach {
            name,
            network_id,
            profile,
            link,
        } => {
            let n = app
                .zt_attach(&name, &network_id, &profile, link.as_deref())
                .await?;
            if ctx.output == Output::Json {
                return print_json(&n);
            }
            println!(
                "Attached ZeroTier network `{}` ({}){}.",
                n.name,
                n.network_id,
                linked(&n)
            );
        }
        ZtCmd::List => {
            let all = app.zt_list().await?;
            if ctx.output == Output::Json {
                return print_json(&all);
            }
            if all.is_empty() {
                println!(
                    "No networks yet. Create one with `ikit net zt create <name> --profile <zerotier profile>`."
                );
                return Ok(());
            }
            let mut t = table(&[
                "NAME",
                "NETWORK ID",
                "CIDR",
                "ORIGIN",
                "PROFILE",
                "WORKLOADS",
            ]);
            for n in &all {
                t.add_row(vec![
                    n.name.clone(),
                    n.network_id.clone(),
                    n.cidr.clone().unwrap_or_default(),
                    n.origin.to_string(),
                    n.profile.clone(),
                    n.workloads.join(", "),
                ]);
            }
            println!("{t}");
        }
        ZtCmd::Show { name } => {
            let d = app.zt_show(&name).await?;
            if ctx.output == Output::Json {
                return print_json(&d);
            }
            let n = &d.network;
            println!("Network:    {} ({})", n.name, n.network_id);
            println!(
                "Central:    {} ({})",
                d.central.name,
                if d.central.private {
                    "private"
                } else {
                    "public"
                }
            );
            println!("Routes:     {}", d.central.routes.join(", "));
            println!("Pools:      {}", d.central.pools.join(", "));
            println!("Origin:     {} via profile {}", n.origin, n.profile);
            println!(
                "Workloads:  {}",
                if n.workloads.is_empty() {
                    "-".to_owned()
                } else {
                    n.workloads.join(", ")
                }
            );
            println!();
            members_table(&d.members);
        }
        ZtCmd::Members { name } => {
            let members = app.zt_members(&name).await?;
            if ctx.output == Output::Json {
                return print_json(&members);
            }
            members_table(&members);
        }
        ZtCmd::Join {
            name,
            target,
            authorize,
            member_name,
        } => {
            let j = app
                .zt_join(&name, target.as_deref(), authorize, member_name.as_deref())
                .await?;
            if ctx.output == Output::Json {
                return print_json(&j);
            }
            let place = j
                .target
                .as_deref()
                .map_or("This machine".to_owned(), |t| format!("Target `{t}`"));
            println!(
                "{place} (node {}) joined `{}` ({}).",
                j.node_id, j.network, j.network_id
            );
            match &j.member {
                Some(m) if !m.ips.is_empty() => {
                    println!("Authorized; address {}.", m.ips.join(", "))
                }
                Some(_) => println!("Authorized; an address is assigned within a few seconds."),
                None => println!(
                    "Not authorized yet: `ikit net zt authorize {} {}`.",
                    j.network, j.node_id
                ),
            }
        }
        ZtCmd::Leave {
            name,
            target,
            deauthorize,
        } => {
            let node = app.zt_leave(&name, target.as_deref(), deauthorize).await?;
            if ctx.output == Output::Table {
                println!(
                    "Node {node} left `{name}`{}.",
                    if deauthorize {
                        " and was deauthorized"
                    } else {
                        ""
                    }
                );
            }
        }
        ZtCmd::Authorize {
            name,
            node,
            member_name,
        } => {
            let m = app
                .zt_authorize(&name, &node, true, member_name.as_deref())
                .await?;
            if ctx.output == Output::Json {
                return print_json(&m);
            }
            println!("Authorized {} on `{name}`.", m.node_id);
        }
        ZtCmd::Deauthorize { name, node } => {
            let m = app.zt_authorize(&name, &node, false, None).await?;
            if ctx.output == Output::Json {
                return print_json(&m);
            }
            println!("Deauthorized {} on `{name}`.", m.node_id);
        }
        ZtCmd::Link { name } => {
            let wl = selected_wl()?;
            let n = app.zt_link(&name, &wl).await?;
            if ctx.output == Output::Json {
                return print_json(&n);
            }
            println!("Linked {wl} to `{}`.", n.name);
        }
        ZtCmd::Unlink => {
            let wl = selected_wl()?;
            app.zt_unlink(&wl).await?;
            if ctx.output == Output::Table {
                println!("Unlinked {wl} from its network.");
            }
        }
        ZtCmd::Status { target } => {
            let target = match target {
                Some(t) => Some(app.target_show(&t).await?),
                None => None,
            };
            let node = app.zt_node(target.as_ref()).await?;
            if ctx.output == Output::Json {
                return print_json(&node);
            }
            println!(
                "Node {} ({}, ZeroTier {})",
                node.node.address,
                if node.node.online {
                    "online"
                } else {
                    "offline"
                },
                node.node.version
            );
            if node.networks.is_empty() {
                println!("No networks joined.");
                return Ok(());
            }
            let known = app.zt_list().await?;
            let mut t = table(&["NETWORK ID", "IKIT NAME", "STATUS", "DEVICE", "ADDRESSES"]);
            for n in &node.networks {
                t.add_row(vec![
                    n.id.clone(),
                    known
                        .iter()
                        .find(|k| k.network_id == n.id)
                        .map(|k| k.name.clone())
                        .unwrap_or_default(),
                    n.status.clone(),
                    n.device.clone(),
                    n.addresses.join(", "),
                ]);
            }
            println!("{t}");
        }
        ZtCmd::Rm { name, delete, yes } => {
            let question = if delete {
                format!(
                    "Remove network {name} and delete it in ZeroTier Central (members lose access)?"
                )
            } else {
                format!("Remove network {name} from ikit (it stays in ZeroTier Central)?")
            };
            if ctx.confirm(&question, yes)? {
                app.zt_remove(&name, delete).await?;
                if ctx.output == Output::Table {
                    println!("Removed network `{name}`.");
                }
            }
        }
    }
    Ok(())
}

fn linked(n: &Network) -> String {
    if n.workloads.is_empty() {
        String::new()
    } else {
        format!(", linked to {}", n.workloads.join(", "))
    }
}

fn members_table(members: &[ikit_net::Member]) {
    if members.is_empty() {
        println!("No members yet.");
        return;
    }
    let mut t = table(&[
        "NODE",
        "NAME",
        "AUTHORIZED",
        "ONLINE",
        "ADDRESSES",
        "VERSION",
    ]);
    for m in members {
        t.add_row(vec![
            m.node_id.clone(),
            m.name.clone(),
            if m.authorized { "yes" } else { "no" }.to_owned(),
            if m.online { "yes" } else { "no" }.to_owned(),
            m.ips.join(", "),
            m.client_version.clone(),
        ]);
    }
    println!("{t}");
}
