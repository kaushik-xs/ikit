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
    /// Route a domain (and optional path prefix) through the target's reverse proxy, with a
    /// certificate from ACME, to a service (its port is published on the host's loopback
    /// address only), to a folder of static files the proxy serves itself (`--static`), or
    /// to an address such as a process on the host (`--to`). Takes effect on the next apply.
    Add {
        /// Service instance (default: asked, unless --static or --to is given).
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
        /// Serve the files of this local folder (e.g. a build's dist/). Each apply copies
        /// changed files to the host. Needs --host.
        #[arg(long = "static", value_name = "FOLDER", conflicts_with_all = ["service", "port", "to"])]
        static_dir: Option<std::path::PathBuf>,
        /// With --static: answer requests for missing files with index.html (single-page apps).
        #[arg(long, requires = "static_dir")]
        spa: bool,
        /// Proxy to an address instead of a service: a port on the host (3000), host:port or
        /// http(s)://host:port. Needs --host.
        #[arg(long, value_name = "ADDRESS", conflicts_with_all = ["service", "port"])]
        to: Option<String>,
        /// With --path: pass the prefix on instead of stripping it, for apps served under a
        /// base path (e.g. a Next.js basePath of /ui/app1). Caddy only.
        #[arg(long, requires = "path")]
        keep_prefix: bool,
    },
    /// Add extra proxy directives to a route (`host` or `host/path`), written into its
    /// Caddyfile or ferron.conf block as they are: headers, auth, rate limits, timeouts. The
    /// proxy checks them on apply and keeps the previous site if they are invalid.
    Snippet {
        route: String,
        /// File with the directives (`-` reads stdin).
        #[arg(long, value_name = "FILE", conflicts_with = "clear", required_unless_present_any = ["clear", "show"])]
        file: Option<std::path::PathBuf>,
        /// Remove the route's snippet.
        #[arg(long)]
        clear: bool,
        /// Print the route's snippet.
        #[arg(long, conflicts_with_all = ["file", "clear"])]
        show: bool,
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
            static_dir,
            spa,
            to,
            keep_prefix,
        } => {
            let other = static_dir.is_some() || to.is_some();
            let service = match service {
                Some(s) => s,
                None if other => String::new(),
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
                        static_dir,
                        spa,
                        to,
                        keep_prefix,
                    },
                )
                .await?;
            if ctx.output == Output::Json {
                return print_json(&route);
            }
            let via = if route.is_service() {
                format!(" (loopback port {})", route.upstream_port)
            } else {
                String::new()
            };
            println!(
                "Routed {} to {}{via}. Run `ikit apply` to put it live.",
                route.url(),
                route.target(),
            );
        }
        RouteCmd::Snippet {
            route,
            file,
            clear,
            show,
        } => {
            if show {
                let list = app.route_list(&wl).await?;
                let r = list
                    .routes
                    .iter()
                    .find(|r| r.key() == route.trim_end_matches('/'))
                    .ok_or_else(|| anyhow::anyhow!("no route {route} in {wl}"))?;
                if ctx.output == Output::Json {
                    return print_json(&r.snippet);
                }
                match &r.snippet {
                    Some(s) => println!("{s}"),
                    None => println!("{} has no snippet.", r.key()),
                }
                return Ok(());
            }
            let text = match (&file, clear) {
                (_, true) => None,
                (Some(f), _) if f.as_os_str() == "-" => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    Some(buf)
                }
                (Some(f), _) => Some(
                    std::fs::read_to_string(f)
                        .map_err(|e| anyhow::anyhow!("reading {}: {e}", f.display()))?,
                ),
                (None, false) => bail!("pass --file <FILE>, --clear or --show"),
            };
            let r = app.route_set_snippet(&wl, &route, text.as_deref()).await?;
            if ctx.output == Output::Json {
                return print_json(&r);
            }
            match r.snippet {
                Some(_) => println!(
                    "Set the snippet of {}. Run `ikit plan` to check it and `ikit apply` to put it live.",
                    r.key()
                ),
                None => println!(
                    "Removed the snippet of {}; it goes on the next apply.",
                    r.key()
                ),
            }
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
        let mut t = table(&["URL", "TO", "UPSTREAM", "TLS", "ORIGIN", "SNIPPET"]);
        for r in &list.routes {
            t.add_row(vec![
                r.url(),
                r.target(),
                if r.is_service() {
                    format!("127.0.0.1:{}", r.upstream_port)
                } else {
                    "-".into()
                },
                r.tls.to_string(),
                r.origin.to_string(),
                if r.snippet.is_some() { "yes" } else { "" }.into(),
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
