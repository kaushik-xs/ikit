//! `ikit route ...`: domains the target's reverse proxy sends to the selected workload.

use anyhow::bail;
use clap::Subcommand;
use ikit_app::{App, RouteList, RouteRequest};
use ikit_core::{Category, TlsMode};
use inquire::Select;

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum RouteCmd {
    /// Route a domain (and optional path prefix) to a service through the target's reverse
    /// proxy, with a certificate from ACME. The port is published on the host's loopback
    /// address only. Takes effect on the next apply.
    Add {
        /// Service instance (default: asked).
        service: Option<String>,
        /// Domain (default: the org's host template under `edge.base_domain`).
        #[arg(long)]
        host: Option<String>,
        /// Path prefix, stripped before proxying, e.g. /api.
        #[arg(long)]
        path: Option<String>,
        /// Container port number or registry port name (default: `http`, else the first).
        #[arg(long)]
        port: Option<String>,
        /// acme (public certificate), internal (the proxy's own CA) or off (plain HTTP).
        #[arg(long)]
        tls: Option<TlsMode>,
    },
    /// List the workload's routes and redirects.
    #[command(alias = "ls")]
    List,
    /// Remove a route: `host` or `host/path`.
    Rm { route: String },
    /// Redirect a domain to a URL, keeping the request path (e.g. www → apex).
    Redirect {
        host: String,
        /// e.g. https://acme.com
        to: String,
        /// 302/307 instead of a permanent redirect.
        #[arg(long)]
        temporary: bool,
        #[arg(long)]
        tls: Option<TlsMode>,
    },
    /// Remove a redirect.
    Unredirect { host: String },
    /// Keep routes in step with the workload's services, at the org's host template.
    #[command(subcommand)]
    Auto(AutoCmd),
}

#[derive(Debug, Subcommand)]
pub enum AutoCmd {
    /// Route every service in these groups that has a port named --port-name.
    On {
        #[arg(long, value_delimiter = ',', default_value = "app")]
        groups: Vec<Category>,
        #[arg(long)]
        port_name: Option<String>,
    },
    /// Stop and remove automatic routes (manual routes stay).
    Off,
}

pub async fn run(ctx: &Ctx, cmd: RouteCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    let wl = app.resolve_workload_name(ctx.wl.as_deref())?;
    match cmd {
        RouteCmd::Add {
            service,
            host,
            path,
            port,
            tls,
        } => {
            let service = match service {
                Some(s) => s,
                None => pick_service(ctx, &app, &wl).await?,
            };
            let route = app
                .route_add(
                    &wl,
                    RouteRequest {
                        service,
                        port,
                        host,
                        path,
                        tls,
                    },
                )
                .await?;
            if ctx.output == Output::Json {
                return print_json(&route);
            }
            println!(
                "Routed {} to {}:{} (loopback port {}). Run `ikit apply` to put it live.",
                route.url(),
                route.service,
                route.port,
                route.upstream_port
            );
        }
        RouteCmd::List => print_list(ctx, &app.route_list(&wl).await?)?,
        RouteCmd::Rm { route } => {
            let r = app.route_remove(&wl, &route).await?;
            if ctx.output == Output::Json {
                return print_json(&r);
            }
            println!(
                "Removed {}; it goes off the proxy on the next apply.",
                r.key()
            );
        }
        RouteCmd::Redirect {
            host,
            to,
            temporary,
            tls,
        } => {
            let r = app.redirect_add(&wl, &host, &to, !temporary, tls).await?;
            if ctx.output == Output::Json {
                return print_json(&r);
            }
            println!("{} redirects to {} from the next apply.", r.host, r.to);
        }
        RouteCmd::Unredirect { host } => {
            app.redirect_remove(&wl, &host).await?;
            if ctx.output == Output::Table {
                println!("Removed the redirect of {host}.");
            }
        }
        RouteCmd::Auto(AutoCmd::On { groups, port_name }) => {
            print_list(ctx, &app.route_auto_on(&wl, groups, port_name).await?)?
        }
        RouteCmd::Auto(AutoCmd::Off) => print_list(ctx, &app.route_auto_off(&wl).await?)?,
    }
    Ok(())
}

async fn pick_service(ctx: &Ctx, app: &App, wl: &str) -> anyhow::Result<String> {
    let names: Vec<String> = app
        .services_list(wl)
        .await?
        .into_iter()
        .map(|s| s.name)
        .collect();
    if names.is_empty() {
        bail!("{wl} has no services; add some with `ikit sg add`");
    }
    if !ctx.interactive {
        bail!("pass the service to route: `ikit route add <service> --host <domain>`");
    }
    Ok(Select::new("Service:", names).prompt()?)
}

fn print_list(ctx: &Ctx, list: &RouteList) -> anyhow::Result<()> {
    if ctx.output == Output::Json {
        return print_json(list);
    }
    if let Some(auto) = &list.auto {
        let groups: Vec<String> = auto.groups.iter().map(|g| g.to_string()).collect();
        println!(
            "Automatic routes: on for {} services with a `{}` port{}.",
            groups.join(", "),
            auto.port_name,
            if auto.excluded.is_empty() {
                String::new()
            } else {
                format!(" (not for {})", auto.excluded.join(", "))
            }
        );
    }
    if list.routes.is_empty() && list.redirects.is_empty() {
        println!(
            "{} has no routes. Add one with `ikit route add <service> --host <domain>`.",
            list.workload
        );
        return Ok(());
    }
    if !list.routes.is_empty() {
        let mut t = table(&["URL", "SERVICE", "PORT", "UPSTREAM", "TLS", "ORIGIN"]);
        for r in &list.routes {
            t.add_row(vec![
                r.url(),
                r.service.clone(),
                r.port.to_string(),
                format!("127.0.0.1:{}", r.upstream_port),
                r.tls.to_string(),
                r.origin.to_string(),
            ]);
        }
        println!("{t}");
    }
    if !list.redirects.is_empty() {
        let mut t = table(&["REDIRECT", "TO", "KIND", "TLS"]);
        for r in &list.redirects {
            t.add_row(vec![
                r.host.clone(),
                r.to.clone(),
                if r.permanent {
                    "permanent"
                } else {
                    "temporary"
                }
                .to_owned(),
                r.tls.to_string(),
            ]);
        }
        println!("{t}");
    }
    Ok(())
}
