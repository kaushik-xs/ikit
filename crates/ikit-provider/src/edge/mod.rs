//! The edge: Caddy or Ferron on a target's host, routing domains to services published on
//! the host's loopback address. Everything here is pure: config rendering, the shell
//! scripts ikit runs on the host, and parsing what they print.
//!
//! Layout on the host:
//!
//! ```text
//! /etc/ikit/edge/host.json            the proxy and the orgs using it (HostManifest)
//! /etc/ikit/edge/.lock                flock'd by every script that changes the edge
//! /etc/ikit/edge/sites/<org>__<wl>.conf   one site file per workload, included by the
//!                                     proxy's main config
//! ```
//!
//! Each site file starts with `# ikit ...` header lines naming its org, workload, routes
//! and redirects, so every org can see which domains and upstream ports are taken.

mod caddy;
mod ferron;
mod host;

use std::collections::{BTreeMap, BTreeSet};

use ikit_core::{EdgeConfig, EdgeKind, Redirect, Route, TlsMode};
use serde::Serialize;

pub use host::{
    HostProbe, InstalledServer, PkgFamily, adopt_script, firewall_script, install_script,
    parse_probe, probe_script, reload_script, remove_site_script, uninstall_script, upgrade_script,
    write_host_script, write_site_script,
};

pub const EDGE_DIR: &str = "/etc/ikit/edge";
pub const SITES_DIR: &str = "/etc/ikit/edge/sites";
pub const HOST_FILE: &str = "/etc/ikit/edge/host.json";
/// An empty site file, so the include glob always matches something.
pub const PLACEHOLDER_SITE: &str = "000-ikit.conf";

/// Per-server facts: package, binary, service and config paths.
pub(crate) struct Server {
    pub package: &'static str,
    pub binary: &'static str,
    pub service: &'static str,
    pub main_config: &'static str,
}

pub(crate) fn server(kind: EdgeKind) -> Server {
    match kind {
        EdgeKind::Caddy => Server {
            package: "caddy",
            binary: "caddy",
            service: "caddy",
            main_config: "/etc/caddy/Caddyfile",
        },
        EdgeKind::Ferron => Server {
            package: "ferron3",
            binary: "ferron",
            service: "ferron",
            main_config: "/etc/ferron/ferron.conf",
        },
    }
}

/// The line in the proxy's main config that pulls in the site files.
pub fn include_line(kind: EdgeKind) -> String {
    match kind {
        EdgeKind::Caddy => format!("import {SITES_DIR}/*.conf"),
        EdgeKind::Ferron => format!("include \"{SITES_DIR}/*.conf\""),
    }
}

/// The main config ikit writes when it installs the proxy itself.
pub fn base_config(kind: EdgeKind) -> String {
    // Ferron refuses a config without directives, which is what the include gives before
    // any site exists; its default listen address keeps it non-empty.
    let global = match kind {
        EdgeKind::Caddy => "",
        EdgeKind::Ferron => "{\n    tcp {\n        listen \"::\"\n    }\n}\n",
    };
    format!(
        "# Managed by ikit. Sites live in {SITES_DIR}, one file per workload;\n\
         # the original config was kept as {main}.ikit-orig.\n{global}{}\n",
        include_line(kind),
        main = server(kind).main_config,
    )
}

/// Shell that checks the full config (main config plus every site file).
pub(crate) fn validate_cmd(kind: EdgeKind) -> String {
    let main = server(kind).main_config;
    match kind {
        EdgeKind::Caddy => format!("caddy validate --config {main} --adapter caddyfile"),
        EdgeKind::Ferron => format!("ferron validate -c {main}"),
    }
}

/// Shell printing the server's version on one line.
pub(crate) fn version_cmd(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Caddy => "caddy version",
        EdgeKind::Ferron => "ferron version",
    }
}

/// The site file of a workload.
pub fn site_file(org: &str, workload: &str) -> String {
    format!("{org}__{workload}.conf")
}

/// One workload's routes and redirects, rendered into its site file.
#[derive(Debug, Clone)]
pub struct SiteInput<'a> {
    pub org: &'a str,
    pub workload: &'a str,
    pub routes: &'a [Route],
    pub redirects: &'a [Redirect],
    /// This org's ACME email and CA.
    pub edge: &'a EdgeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EdgeRenderError {
    #[error("routes on {host} use different TLS modes ({a} and {b}); use one per host")]
    MixedTls {
        host: String,
        a: TlsMode,
        b: TlsMode,
    },
    #[error("{0} is both routed and redirected; remove one")]
    RoutedAndRedirected(String),
}

/// A host's routes, longest path first so the most specific prefix wins.
pub(crate) struct HostRoutes<'a> {
    pub host: &'a str,
    pub tls: TlsMode,
    pub routes: Vec<&'a Route>,
}

fn group_by_host<'a>(routes: &'a [Route]) -> Result<Vec<HostRoutes<'a>>, EdgeRenderError> {
    let mut hosts: BTreeMap<&str, Vec<&Route>> = BTreeMap::new();
    for r in routes {
        hosts.entry(&r.host).or_default().push(r);
    }
    hosts
        .into_iter()
        .map(|(host, mut routes)| {
            let tls = routes[0].tls;
            if let Some(other) = routes.iter().find(|r| r.tls != tls) {
                return Err(EdgeRenderError::MixedTls {
                    host: host.to_owned(),
                    a: tls,
                    b: other.tls,
                });
            }
            routes.sort_by_key(|r| std::cmp::Reverse(r.path.as_deref().map_or(0, |p| p.len())));
            Ok(HostRoutes { host, tls, routes })
        })
        .collect()
}

/// The site file for one workload: the `# ikit` header, then the server's config.
pub fn render_site(kind: EdgeKind, input: &SiteInput<'_>) -> Result<String, EdgeRenderError> {
    let hosts = group_by_host(input.routes)?;
    if let Some(r) = input
        .redirects
        .iter()
        .find(|r| hosts.iter().any(|h| h.host == r.host))
    {
        return Err(EdgeRenderError::RoutedAndRedirected(r.host.clone()));
    }
    let header = SiteHeader {
        org: input.org.to_owned(),
        workload: input.workload.to_owned(),
        routes: input
            .routes
            .iter()
            .map(|r| HeaderRoute {
                host: r.host.clone(),
                path: r.path.clone(),
                upstream: r.upstream_port,
                tls: r.tls,
            })
            .collect(),
        redirects: input.redirects.iter().map(|r| r.host.clone()).collect(),
    };
    let body = match kind {
        EdgeKind::Caddy => caddy::render(&hosts, input.redirects, input.edge),
        EdgeKind::Ferron => ferron::render(&hosts, input.redirects, input.edge),
    };
    Ok(format!("{}{body}", header.render()))
}

/// What a site file's header says about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SiteHeader {
    pub org: String,
    pub workload: String,
    pub routes: Vec<HeaderRoute>,
    pub redirects: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeaderRoute {
    pub host: String,
    pub path: Option<String>,
    pub upstream: u16,
    pub tls: TlsMode,
}

impl HeaderRoute {
    pub fn key(&self) -> String {
        ikit_core::edge::route_key(&self.host, self.path.as_deref())
    }
}

impl SiteHeader {
    fn render(&self) -> String {
        let mut s = format!("# ikit site {} {}\n", self.org, self.workload);
        for r in &self.routes {
            s.push_str(&format!(
                "# ikit route {} {} {} {}\n",
                r.host,
                r.path.as_deref().unwrap_or("/"),
                r.upstream,
                r.tls
            ));
        }
        for host in &self.redirects {
            s.push_str(&format!("# ikit redirect {host}\n"));
        }
        s.push_str("# Managed by ikit: changes are overwritten by the next `ikit apply`.\n\n");
        s
    }

    /// Every host the site answers for.
    pub fn hosts(&self) -> BTreeSet<&str> {
        self.routes
            .iter()
            .map(|r| r.host.as_str())
            .chain(self.redirects.iter().map(String::as_str))
            .collect()
    }

    /// Who owns the site, e.g. `wl-acme-dev-001 (org acme)`.
    pub fn owner(&self) -> String {
        format!("{} (org {})", self.workload, self.org)
    }
}

/// Reads the `# ikit` header lines of a site file; `None` if it has no `site` line.
pub fn parse_site_header(text: &str) -> Option<SiteHeader> {
    let mut header: Option<SiteHeader> = None;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("# ikit ") else {
            continue;
        };
        let words: Vec<&str> = rest.split_whitespace().collect();
        match (words.as_slice(), header.as_mut()) {
            (["site", org, workload], None) => {
                header = Some(SiteHeader {
                    org: (*org).to_owned(),
                    workload: (*workload).to_owned(),
                    routes: vec![],
                    redirects: vec![],
                });
            }
            (["route", host, path, upstream, tls], Some(h)) => {
                let (Ok(upstream), Ok(tls)) = (upstream.parse(), tls.parse()) else {
                    continue;
                };
                h.routes.push(HeaderRoute {
                    host: (*host).to_owned(),
                    path: (*path != "/").then(|| (*path).to_owned()),
                    upstream,
                    tls,
                });
            }
            (["redirect", host], Some(h)) => h.redirects.push((*host).to_owned()),
            _ => {}
        }
    }
    header
}

/// One change `apply` would make to the workload's site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum EdgeChange {
    Add { site: String, detail: String },
    Update { site: String, detail: String },
    Remove { site: String },
}

impl EdgeChange {
    pub fn site(&self) -> &str {
        match self {
            Self::Add { site, .. } | Self::Update { site, .. } | Self::Remove { site } => site,
        }
    }
}

/// The site file on the target vs the rendered one (`None`: the workload has no routes).
pub fn diff_site(applied: Option<&str>, rendered: Option<&str>) -> Vec<EdgeChange> {
    if applied == rendered {
        return vec![];
    }
    let entries = |text: Option<&str>| -> BTreeMap<String, String> {
        let Some(h) = text.and_then(parse_site_header) else {
            return BTreeMap::new();
        };
        h.routes
            .iter()
            .map(|r| {
                (
                    r.key(),
                    format!("→ 127.0.0.1:{} (tls {})", r.upstream, r.tls),
                )
            })
            .chain(
                h.redirects
                    .iter()
                    .map(|host| (host.clone(), "redirect".to_owned())),
            )
            .collect()
    };
    let (old, new) = (entries(applied), entries(rendered));
    let mut changes = Vec::new();
    for (site, detail) in &new {
        match old.get(site) {
            None => changes.push(EdgeChange::Add {
                site: site.clone(),
                detail: detail.clone(),
            }),
            Some(o) if o != detail => changes.push(EdgeChange::Update {
                site: site.clone(),
                detail: format!("{o} ⇒ {detail}"),
            }),
            Some(_) => {}
        }
    }
    for site in old.keys().filter(|s| !new.contains_key(*s)) {
        changes.push(EdgeChange::Remove { site: site.clone() });
    }
    if changes.is_empty() {
        // Same routes, different settings (ACME email, CA, redirect target).
        changes.push(EdgeChange::Update {
            site: "*".into(),
            detail: "proxy settings".into(),
        });
    }
    changes
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::Utc;
    use ikit_core::{AcmeCa, RouteOrigin};

    use super::*;

    pub fn route(host: &str, path: Option<&str>, upstream: u16, tls: TlsMode) -> Route {
        Route {
            id: 0,
            service: "web".into(),
            port: 8080,
            host: host.into(),
            path: path.map(str::to_owned),
            tls,
            upstream_port: upstream,
            origin: RouteOrigin::Manual,
            created_at: Utc::now(),
        }
    }

    pub fn redirect(host: &str, to: &str) -> Redirect {
        Redirect {
            id: 0,
            host: host.into(),
            to: to.into(),
            permanent: true,
            tls: TlsMode::Acme,
            created_at: Utc::now(),
        }
    }

    pub fn edge(kind: EdgeKind) -> EdgeConfig {
        EdgeConfig {
            kind,
            email: Some("ops@acme.com".into()),
            ca: AcmeCa::Production,
            version: None,
        }
    }

    #[test]
    fn header_round_trips() {
        let routes = [
            route("acme.com", None, 20000, TlsMode::Acme),
            route("acme.com", Some("/api"), 20001, TlsMode::Acme),
        ];
        let redirects = [redirect("www.acme.com", "https://acme.com")];
        let e = edge(EdgeKind::Caddy);
        let input = SiteInput {
            org: "acme",
            workload: "wl-acme-dev-001",
            routes: &routes,
            redirects: &redirects,
            edge: &e,
        };
        let text = render_site(EdgeKind::Caddy, &input).unwrap();
        let h = parse_site_header(&text).unwrap();
        assert_eq!(h.owner(), "wl-acme-dev-001 (org acme)");
        assert_eq!(
            h.routes.iter().map(HeaderRoute::key).collect::<Vec<_>>(),
            ["acme.com", "acme.com/api"]
        );
        assert_eq!(h.routes[1].upstream, 20001);
        assert_eq!(
            h.hosts().into_iter().collect::<Vec<_>>(),
            ["acme.com", "www.acme.com"]
        );
        assert_eq!(parse_site_header("# just a comment\n"), None);
    }

    #[test]
    fn refuses_mixed_tls_and_routed_redirects() {
        let e = edge(EdgeKind::Caddy);
        let routes = [
            route("acme.com", None, 20000, TlsMode::Acme),
            route("acme.com", Some("/api"), 20001, TlsMode::Off),
        ];
        let input = SiteInput {
            org: "acme",
            workload: "w",
            routes: &routes,
            redirects: &[],
            edge: &e,
        };
        assert!(matches!(
            render_site(EdgeKind::Caddy, &input),
            Err(EdgeRenderError::MixedTls { .. })
        ));
        let routes = [route("acme.com", None, 20000, TlsMode::Acme)];
        let redirects = [redirect("acme.com", "https://example.com")];
        let input = SiteInput {
            routes: &routes,
            redirects: &redirects,
            ..input
        };
        assert!(render_site(EdgeKind::Ferron, &input).is_err());
    }

    #[test]
    fn diffs_sites() {
        let e = edge(EdgeKind::Caddy);
        let render = |routes: &[Route], e: &EdgeConfig| {
            render_site(
                EdgeKind::Caddy,
                &SiteInput {
                    org: "acme",
                    workload: "w",
                    routes,
                    redirects: &[],
                    edge: e,
                },
            )
            .unwrap()
        };
        let a = render(&[route("a.acme.com", None, 20000, TlsMode::Acme)], &e);
        let b = render(
            &[
                route("a.acme.com", None, 20005, TlsMode::Acme),
                route("b.acme.com", None, 20001, TlsMode::Internal),
            ],
            &e,
        );
        assert!(diff_site(Some(&a), Some(&a)).is_empty());
        let changes = diff_site(Some(&a), Some(&b));
        assert_eq!(
            changes.iter().map(EdgeChange::site).collect::<Vec<_>>(),
            ["a.acme.com", "b.acme.com"]
        );
        assert!(matches!(changes[0], EdgeChange::Update { .. }));
        assert!(matches!(changes[1], EdgeChange::Add { .. }));
        assert_eq!(
            diff_site(Some(&a), None),
            vec![EdgeChange::Remove {
                site: "a.acme.com".into()
            }]
        );
        let staging = EdgeConfig {
            ca: AcmeCa::Staging,
            ..e.clone()
        };
        let c = render(&[route("a.acme.com", None, 20000, TlsMode::Acme)], &staging);
        assert_eq!(diff_site(Some(&a), Some(&c))[0].site(), "*");
    }

    /// Writes a sample site, the base config and the host scripts for both servers to
    /// `$IKIT_EDGE_OUT/{caddy,ferron}/`, to check them against the real binaries, e.g.
    /// `docker run --rm -v $OUT/caddy/main.conf:/etc/caddy/Caddyfile -v
    /// $OUT/caddy/sites:/etc/ikit/edge/sites caddy:2 caddy validate --config
    /// /etc/caddy/Caddyfile`, or by running `install_apt.sh` in a `debian:12` container.
    #[test]
    #[ignore = "writes files for checking against real Caddy and Ferron"]
    fn dump_for_real_validation() {
        let out = std::path::PathBuf::from(std::env::var("IKIT_EDGE_OUT").unwrap());
        let routes = [
            route("acme.com", None, 20000, TlsMode::Acme),
            route("acme.com", Some("/api"), 20001, TlsMode::Acme),
            route("admin.acme.com", None, 20002, TlsMode::Internal),
            route("plain.acme.com", Some("/v1"), 20003, TlsMode::Off),
        ];
        let mut redirects = [redirect("www.acme.com", "https://acme.com")];
        redirects[0].permanent = false;
        for kind in EdgeKind::ALL {
            let e = EdgeConfig {
                ca: ikit_core::AcmeCa::Staging,
                ..edge(*kind)
            };
            let text = render_site(
                *kind,
                &SiteInput {
                    org: "acme",
                    workload: "wl-acme-dev-001",
                    routes: &routes,
                    redirects: &redirects,
                    edge: &e,
                },
            )
            .unwrap();
            let dir = out.join(kind.as_str());
            std::fs::write(dir.join("sites/acme__wl-acme-dev-001.conf"), text).unwrap();
            std::fs::write(dir.join("sites/000-ikit.conf"), "# placeholder\n").unwrap();
            std::fs::write(dir.join("main.conf"), base_config(*kind)).unwrap();
            let m = ikit_core::HostManifest {
                kind: *kind,
                installed_by: "acme".into(),
                adopted: true,
                exclusive: false,
                orgs: vec!["acme".into()],
                version: None,
                installed_at: chrono::Utc::now(),
                upgraded_at: None,
            };
            let scripts = [
                ("probe.sh", probe_script()),
                ("adopt.sh", adopt_script(*kind, &m)),
                (
                    "write_site.sh",
                    write_site_script(*kind, "acme__w.conf", &["acme.com", "www.acme.com"]),
                ),
                ("remove_site.sh", remove_site_script(*kind, "acme__w.conf")),
                (
                    "uninstall.sh",
                    uninstall_script(*kind, PkgFamily::Apt, true, false),
                ),
                (
                    "install_apt.sh",
                    install_script(*kind, PkgFamily::Apt, None, &m),
                ),
                (
                    "install_dnf.sh",
                    install_script(*kind, PkgFamily::Dnf, None, &m),
                ),
                (
                    "upgrade_apt.sh",
                    upgrade_script(*kind, PkgFamily::Apt, None),
                ),
                (
                    "upgrade_dnf.sh",
                    upgrade_script(*kind, PkgFamily::Dnf, None),
                ),
                (
                    "purge_apt.sh",
                    uninstall_script(*kind, PkgFamily::Apt, false, true),
                ),
                (
                    "purge_dnf.sh",
                    uninstall_script(*kind, PkgFamily::Dnf, false, true),
                ),
            ];
            for (name, script) in scripts {
                std::fs::write(dir.join(name), script).unwrap();
            }
        }
    }
}
