//! Ferron 3 host blocks (`ferron.conf` syntax). A host block listens on 80 and 443 with
//! automatic TLS and redirects HTTP to HTTPS; `tls false` with `https_redirect false`
//! serves plain HTTP only. (`host:80` would serve TLS on port 80.) `location` blocks match
//! path prefixes and strip them before proxying.

use ikit_core::{EdgeConfig, Redirect, TlsMode};

use super::{Handler, HostRoutes, Served, indented};

fn tls_block(tls: TlsMode, edge: &EdgeConfig) -> String {
    match tls {
        TlsMode::Off => "    tls false\n    https_redirect false\n".to_owned(),
        TlsMode::Internal => "    tls {\n        provider local\n    }\n".to_owned(),
        TlsMode::Acme => {
            let mut lines = String::new();
            if let Some(email) = &edge.email {
                lines.push_str(&format!("        contact {email}\n"));
            }
            if let Some(dir) = edge.ca.directory() {
                lines.push_str(&format!("        directory {dir}\n"));
            }
            if lines.is_empty() {
                return String::new();
            }
            format!("    tls {{\n        provider acme\n{lines}    }}\n")
        }
    }
}

/// The directives that answer a route's requests, indented by `indent`.
fn handle_body(r: &Served<'_>, indent: &str) -> String {
    let mut out = String::new();
    match &r.handler {
        Handler::Proxy(to) => {
            let to = if to.contains("://") {
                to.clone()
            } else {
                format!("http://{to}")
            };
            out.push_str(&format!("{indent}proxy {to}\n"));
        }
        Handler::Files { root, spa } => {
            out.push_str(&format!("{indent}root \"{root}\"\n"));
            if *spa {
                // Requests that match no file or folder get the index page.
                out.push_str(&format!(
                    "{indent}rewrite r\"^/.*\" \"/\" {{\n\
                     {indent}    last\n\
                     {indent}    directory false\n\
                     {indent}    file false\n\
                     {indent}}}\n"
                ));
            }
        }
    }
    if let Some(snippet) = r.snippet {
        out.push_str(&indented(snippet, indent));
    }
    out
}

pub(super) fn render(
    hosts: &[HostRoutes<'_>],
    redirects: &[Redirect],
    edge: &EdgeConfig,
) -> String {
    let mut out = String::new();
    for h in hosts {
        out.push_str(&format!("{} {{\n", h.host));
        out.push_str(&tls_block(h.tls, edge));
        // Ferron applies a whole-host route's directives (and `location /`) inside every
        // location too. A lone route, or routes that all just proxy (a location's proxy
        // wins), can stay at the host level; otherwise the whole-host route is guarded so
        // it only sees requests no path route takes.
        let paths: Vec<&str> = h.routes.iter().filter_map(|r| r.path).collect();
        let guard = !paths.is_empty() && !h.plain_proxies();
        if guard && h.routes.iter().any(|r| r.path.is_none()) {
            let alternatives: Vec<String> = paths
                .iter()
                .map(|p| p.trim_start_matches('/').replace('.', "\\."))
                .collect();
            out.push_str(&format!(
                "    match ikit_paths {{\n        request.uri.path ~ r\"^/({})(/|$)\"\n    }}\n",
                alternatives.join("|")
            ));
        }
        for r in &h.routes {
            match r.path {
                Some(path) => out.push_str(&format!(
                    "    location {path} {{\n{}    }}\n",
                    handle_body(r, "        ")
                )),
                None if !guard => out.push_str(&handle_body(r, "    ")),
                None => out.push_str(&format!(
                    "    if_not ikit_paths {{\n{}    }}\n",
                    handle_body(r, "        ")
                )),
            }
        }
        out.push_str("}\n\n");
    }
    for r in redirects {
        out.push_str(&format!("{} {{\n", r.host));
        out.push_str(&tls_block(r.tls, edge));
        out.push_str(&format!(
            "    status {} {{\n        location \"{}{{{{request.uri}}}}\"\n    }}\n}}\n\n",
            if r.permanent { 308 } else { 307 },
            r.to
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use ikit_core::{AcmeCa, EdgeKind};

    use super::super::tests::{address_route, edge, hashes, redirect, route, static_route};
    use super::super::{SiteInput, render_site};
    use super::*;
    use ikit_core::Route;
    use std::collections::BTreeMap;

    #[test]
    fn renders_hosts_paths_and_redirects() {
        let routes = [
            route("acme.com", None, 20000, TlsMode::Acme),
            route("acme.com", Some("/api"), 20001, TlsMode::Acme),
            route("admin.acme.com", None, 20002, TlsMode::Internal),
            route("plain.acme.com", None, 20003, TlsMode::Off),
        ];
        let mut redirects = [redirect("www.acme.com", "https://acme.com")];
        redirects[0].permanent = false;
        let e = EdgeConfig {
            ca: AcmeCa::Staging,
            ..edge(EdgeKind::Ferron)
        };
        let text = render_site(
            EdgeKind::Ferron,
            &SiteInput {
                org: "acme",
                workload: "wl-acme-dev-001",
                routes: &routes,
                redirects: &redirects,
                edge: &e,
                static_hashes: &BTreeMap::new(),
            },
        )
        .unwrap();
        let body = text.split_once("next `ikit apply`.\n\n").unwrap().1;
        let tls = "    tls {
        provider acme
        contact ops@acme.com
        directory https://acme-staging-v02.api.letsencrypt.org/directory
    }
";
        assert_eq!(
            body,
            format!(
                "acme.com {{
{tls}    location /api {{
        proxy http://127.0.0.1:20001
    }}
    proxy http://127.0.0.1:20000
}}

admin.acme.com {{
    tls {{
        provider local
    }}
    proxy http://127.0.0.1:20002
}}

plain.acme.com {{
    tls false
    https_redirect false
    proxy http://127.0.0.1:20003
}}

www.acme.com {{
{tls}    status 307 {{
        location \"https://acme.com{{{{request.uri}}}}\"
    }}
}}

"
            )
        );
    }

    #[test]
    fn guards_the_whole_host_route_next_to_files() {
        let off = |r: Route| Route {
            tls: TlsMode::Off,
            ..r
        };
        let routes = [
            off(address_route(
                "site.acme.com",
                None,
                "http://127.0.0.1:3000",
            )),
            off(static_route("site.acme.com", Some("/v1.0"), true)),
            off(static_route("solo.acme.com", None, false)),
            // All proxies: rendered as before, whole-host proxy at the host level.
            off(route("api.acme.com", None, 20000, TlsMode::Off)),
            off(route("api.acme.com", Some("/v2"), 20001, TlsMode::Off)),
        ];
        let e = edge(EdgeKind::Ferron);
        let text = render_site(
            EdgeKind::Ferron,
            &SiteInput {
                org: "acme",
                workload: "w",
                routes: &routes,
                redirects: &[],
                edge: &e,
                static_hashes: &hashes(&routes),
            },
        )
        .unwrap();
        let body = text.split_once("next `ikit apply`.\n\n").unwrap().1;
        let www = "/var/lib/ikit/edge/www/acme/w";
        let plain = "    tls false\n    https_redirect false\n";
        assert_eq!(
            body,
            format!(
                "api.acme.com {{
{plain}    location /v2 {{
        proxy http://127.0.0.1:20001
    }}
    proxy http://127.0.0.1:20000
}}

site.acme.com {{
{plain}    match ikit_paths {{
        request.uri.path ~ r\"^/(v1\\.0)(/|$)\"
    }}
    location /v1.0 {{
        root \"{www}/site.acme.com+v1.0/abcdef0123456789\"
        rewrite r\"^/.*\" \"/\" {{
            last
            directory false
            file false
        }}
    }}
    if_not ikit_paths {{
        proxy http://127.0.0.1:3000
    }}
}}

solo.acme.com {{
{plain}    root \"{www}/solo.acme.com/abcdef0123456789\"
}}

"
            )
        );
    }

    #[test]
    fn production_without_email_needs_no_tls_block() {
        let e = EdgeConfig {
            email: None,
            ..edge(EdgeKind::Ferron)
        };
        assert_eq!(tls_block(TlsMode::Acme, &e), "");
    }
}
