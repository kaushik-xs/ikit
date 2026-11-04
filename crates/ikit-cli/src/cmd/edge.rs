//! `ikit target edge ...`: the reverse proxy (Caddy or Ferron) on a target's host.

use anyhow::bail;
use clap::Subcommand;
use ikit_app::{App, EdgeInstallAction, EdgeInstallRequest, EdgeStatus, EdgeUninstallAction};
use ikit_core::{AcmeCa, EdgeKind};
use inquire::{Select, Text};

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum EdgeCmd {
    /// Set up a reverse proxy on the target's host for this org: install it, adopt one that
    /// is already installed, or join the one another org runs there. Ports 80/443 go to it,
    /// and routes (`ikit route add`) get certificates automatically.
    Install {
        target: String,
        /// caddy or ferron (default: the one already on the host, else asked).
        kind: Option<EdgeKind>,
        /// ACME account email for this org's certificates.
        #[arg(long)]
        email: Option<String>,
        /// Use Let's Encrypt staging (untrusted certificates, for trying things out).
        #[arg(long)]
        staging: bool,
        /// Package version to install (as `apt-cache policy` or `dnf list` shows it).
        #[arg(long)]
        version: Option<String>,
        /// Keep other orgs off this proxy.
        #[arg(long)]
        exclusive: bool,
        /// Allow ports 80 and 443 in ufw or firewalld.
        #[arg(long)]
        open_firewall: bool,
        #[arg(long, short)]
        yes: bool,
    },
    /// Show the proxy, the orgs using it and the sites it serves.
    Status { target: String },
    /// Upgrade the proxy's package (every org on the host is affected). The previous
    /// version is reinstalled if the new one rejects the config or does not start.
    Upgrade {
        target: String,
        /// Package version (default: the newest).
        #[arg(long)]
        version: Option<String>,
        #[arg(long, short)]
        yes: bool,
    },
    /// Check the proxy's config and reload it.
    Reload { target: String },
    /// Take this org off the proxy. The last org removes ikit from it: a proxy ikit
    /// installed is stopped, an adopted one keeps running without ikit's sites.
    Uninstall {
        target: String,
        /// Also remove the package (not for adopted proxies).
        #[arg(long)]
        purge: bool,
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(ctx: &Ctx, app: &App, cmd: EdgeCmd) -> anyhow::Result<()> {
    match cmd {
        EdgeCmd::Install {
            target,
            kind,
            email,
            staging,
            version,
            exclusive,
            open_firewall,
            yes,
        } => {
            let kind = match kind {
                Some(k) => k,
                None => pick_kind(ctx, app, &target).await?,
            };
            let email = match email {
                Some(e) => Some(e),
                None if ctx.interactive => {
                    let e = Text::new("ACME email for certificate notices (optional):").prompt()?;
                    (!e.trim().is_empty()).then(|| e.trim().to_owned())
                }
                None => None,
            };
            if !ctx.confirm(
                &format!("Set up {kind} on {target}? It takes over ports 80 and 443."),
                yes,
            )? {
                return Ok(());
            }
            if ctx.output == Output::Table {
                println!("Setting up {kind} on {target}...");
            }
            let report = app
                .edge_install(
                    &target,
                    EdgeInstallRequest {
                        kind,
                        email,
                        ca: if staging {
                            AcmeCa::Staging
                        } else {
                            AcmeCa::Production
                        },
                        version,
                        exclusive,
                        open_firewall,
                    },
                )
                .await?;
            if ctx.output == Output::Json {
                return print_json(&report);
            }
            let version = report
                .version
                .as_deref()
                .map(|v| format!(" {v}"))
                .unwrap_or_default();
            let what = match report.action {
                EdgeInstallAction::Installed => "Installed",
                EdgeInstallAction::Adopted => "Adopted the existing",
                EdgeInstallAction::Joined => "Joined",
                EdgeInstallAction::Updated => "Updated this org's settings for",
            };
            println!("{what} {}{version} on {target}.", report.kind);
            if report.orgs.len() > 1 {
                println!("Orgs using it: {}.", report.orgs.join(", "));
            }
            if report.action == EdgeInstallAction::Adopted {
                println!(
                    "ikit added one include line to the existing config and leaves the rest alone."
                );
            }
            if let Some(fw) = &report.firewall {
                println!("Firewall: {fw}.");
            }
            println!(
                "Add routes with `ikit route add <service> --host <domain>`, then `ikit apply`."
            );
        }
        EdgeCmd::Status { target } => {
            let status = app.edge_status(&target).await?;
            if ctx.output == Output::Json {
                return print_json(&status);
            }
            print_status(&status);
        }
        EdgeCmd::Upgrade {
            target,
            version,
            yes,
        } => {
            let status = app.edge_status(&target).await?;
            let Some(m) = &status.manifest else {
                bail!("there is no ikit reverse proxy on {target}");
            };
            let to = version.as_deref().unwrap_or("the newest version");
            let others: Vec<&str> = m
                .orgs
                .iter()
                .filter(|o| **o != app.org().name)
                .map(String::as_str)
                .collect();
            let also = if others.is_empty() {
                String::new()
            } else {
                format!(" This also restarts it for org(s) {}.", others.join(", "))
            };
            if !ctx.confirm(
                &format!(
                    "Upgrade {} {} on {target} to {to}?{also}",
                    m.kind,
                    m.version.as_deref().unwrap_or("")
                ),
                yes,
            )? {
                return Ok(());
            }
            let report = app.edge_upgrade(&target, version.as_deref()).await?;
            if ctx.output == Output::Json {
                return print_json(&report);
            }
            match (&report.previous, &report.version) {
                (Some(a), Some(b)) if a == b => {
                    println!("{} on {target} is already at {b}.", report.kind)
                }
                (a, b) => println!(
                    "Upgraded {} on {target}: {} → {}.",
                    report.kind,
                    a.as_deref().unwrap_or("?"),
                    b.as_deref().unwrap_or("?")
                ),
            }
        }
        EdgeCmd::Reload { target } => {
            app.edge_reload(&target).await?;
            if ctx.output == Output::Table {
                println!("Reloaded the reverse proxy on {target}.");
            }
        }
        EdgeCmd::Uninstall { target, purge, yes } => {
            if !ctx.confirm(
                &format!(
                    "Take org {} off the reverse proxy on {target}?",
                    app.org().name
                ),
                yes,
            )? {
                return Ok(());
            }
            let report = app.edge_uninstall(&target, purge).await?;
            if ctx.output == Output::Json {
                return print_json(&report);
            }
            match (report.action, report.adopted) {
                (EdgeUninstallAction::Left, _) => println!(
                    "Org {} left the {} proxy on {target}; other orgs keep using it.",
                    app.org().name,
                    report.kind
                ),
                (EdgeUninstallAction::Removed, true) => println!(
                    "Removed ikit's include line from {} on {target}; it keeps running as before.",
                    report.kind
                ),
                (EdgeUninstallAction::Removed, false) => println!(
                    "Stopped {} on {target}{}.",
                    report.kind,
                    if purge {
                        " and removed its package"
                    } else {
                        ""
                    }
                ),
            }
        }
    }
    Ok(())
}

/// The proxy already on the host, or the deployer's pick.
async fn pick_kind(ctx: &Ctx, app: &App, target: &str) -> anyhow::Result<EdgeKind> {
    let probe = app.edge_probe(target).await?;
    if let Some(Ok(m)) = &probe.manifest {
        if ctx.output == Output::Table {
            println!(
                "{target} runs {} through ikit (installed by org {}); joining it.",
                m.kind, m.installed_by
            );
        }
        return Ok(m.kind);
    }
    let installed: Vec<EdgeKind> = probe.servers.keys().copied().collect();
    if let [one] = installed.as_slice() {
        if ctx.output == Output::Table {
            println!("{one} is already installed on {target}; adopting it.");
        }
        return Ok(*one);
    }
    if !ctx.interactive {
        bail!("pass the proxy to set up: `ikit target edge install {target} caddy` (or ferron)");
    }
    let options = vec![
        "caddy   (mature, Caddyfile, automatic HTTPS)",
        "ferron  (Rust, ferron.conf, automatic HTTPS)",
    ];
    let choice = Select::new("Reverse proxy:", options).prompt()?;
    Ok(if choice.starts_with("caddy") {
        EdgeKind::Caddy
    } else {
        EdgeKind::Ferron
    })
}

fn print_status(s: &EdgeStatus) {
    let mut t = table(&[]);
    match &s.manifest {
        Some(m) => {
            t.add_row(vec![
                "proxy".to_owned(),
                format!(
                    "{} {}{}",
                    m.kind,
                    s.server
                        .as_ref()
                        .map(|v| v.version.clone())
                        .or(m.version.clone())
                        .unwrap_or_default(),
                    if m.adopted { " (adopted)" } else { "" }
                ),
            ]);
            t.add_row(vec![
                "running".to_owned(),
                s.server
                    .as_ref()
                    .map_or("not installed", |v| if v.active { "yes" } else { "no" })
                    .to_owned(),
            ]);
            t.add_row(vec![
                "orgs".to_owned(),
                format!(
                    "{}{}",
                    m.orgs.join(", "),
                    if m.exclusive { " (exclusive)" } else { "" }
                ),
            ]);
        }
        None => {
            t.add_row(vec!["proxy".to_owned(), "none set up by ikit".to_owned()]);
        }
    }
    if let Some(c) = &s.config {
        t.add_row(vec![
            "this org".to_owned(),
            format!("email {}, {} CA", c.email.as_deref().unwrap_or("-"), c.ca),
        ]);
    }
    if !s.listeners.is_empty() {
        t.add_row(vec!["listening".to_owned(), s.listeners.join("\n")]);
    }
    println!("{t}");
    if let Some(p) = &s.problem {
        crate::warn(p);
    }
    if !s.sites.is_empty() {
        let mut t = table(&["SITE FILE", "ORG", "WORKLOAD", "DOMAINS"]);
        for site in &s.sites {
            t.add_row(vec![
                site.file.clone(),
                site.org.clone().unwrap_or("?".into()) + if site.mine { "" } else { " (other)" },
                site.workload.clone().unwrap_or_default(),
                site.hosts.join("\n"),
            ]);
        }
        println!("{t}");
    }
}
