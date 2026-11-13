//! Caddyfile site blocks. Caddy redirects HTTP to HTTPS by itself and gets certificates
//! for every site address that is a public domain.

use ikit_core::{EdgeConfig, Redirect, TlsMode};

use super::{Handler, HostRoutes, Served, indented};

fn address(host: &str, tls: TlsMode) -> String {
    match tls {
        TlsMode::Off => format!("http://{host}"),
        _ => host.to_owned(),
    }
}

/// The `tls` directive for a site, if it needs one.
fn tls_directive(tls: TlsMode, edge: &EdgeConfig) -> Option<String> {
    match tls {
        TlsMode::Off => None,
        TlsMode::Internal => Some("\ttls internal\n".to_owned()),
        TlsMode::Acme => match (edge.email.as_deref(), edge.ca.directory()) {
            (None, None) => None,
            (Some(email), None) => Some(format!("\ttls {email}\n")),
            (email, Some(dir)) => Some(format!(
                "\ttls{} {{\n\t\tca {dir}\n\t}}\n",
                email.map(|e| format!(" {e}")).unwrap_or_default()
            )),
        },
    }
}

/// The directives that answer a route's requests, indented by `indent`.
fn handle_body(r: &Served<'_>, indent: &str) -> String {
    let mut out = String::new();
    if let Some(path) = r.path
        && !r.keep_prefix
    {
        out.push_str(&format!("{indent}uri strip_prefix {path}\n"));
    }
    match &r.handler {
        Handler::Proxy(to) => out.push_str(&format!("{indent}reverse_proxy {to}\n")),
        Handler::Files { root, spa } => {
            out.push_str(&format!("{indent}root * {root}\n"));
            if *spa {
                out.push_str(&format!("{indent}try_files {{path}} /index.html\n"));
            }
            out.push_str(&format!("{indent}file_server\n"));
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
        out.push_str(&format!("{} {{\n", address(h.host, h.tls)));
        out.extend(tls_directive(h.tls, edge));
        let only_root = h.routes.len() == 1 && h.routes[0].path.is_none();
        if only_root {
            out.push_str(&handle_body(&h.routes[0], "\t"));
        } else {
            for (i, r) in h.routes.iter().enumerate() {
                match r.path {
                    Some(path) => out.push_str(&format!(
                        "\t@r{i} path {path} {path}/*\n\thandle @r{i} {{\n{}\t}}\n",
                        handle_body(r, "\t\t")
                    )),
                    None => out.push_str(&format!("\thandle {{\n{}\t}}\n", handle_body(r, "\t\t"))),
                }
            }
            if h.routes.iter().all(|r| r.path.is_some()) {
                out.push_str("\thandle {\n\t\trespond 404\n\t}\n");
            }
        }
        out.push_str("}\n\n");
    }
    for r in redirects {
        out.push_str(&format!("{} {{\n", address(&r.host, r.tls)));
        out.extend(tls_directive(r.tls, edge));
        out.push_str(&format!(
            "\tredir {}{{uri}} {}\n}}\n\n",
            r.to,
            if r.permanent {
                "permanent"
            } else {
                "temporary"
            }
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

    fn body(text: &str) -> &str {
        text.split_once("next `ikit apply`.\n\n").unwrap().1
    }

    #[test]
    fn renders_hosts_paths_and_redirects() {
        let routes = [
            route("acme.com", None, 20000, TlsMode::Acme),
            route("acme.com", Some("/api"), 20001, TlsMode::Acme),
            route("admin.acme.com", None, 20002, TlsMode::Internal),
            route("plain.acme.com", Some("/v1"), 20003, TlsMode::Off),
        ];
        let redirects = [redirect("www.acme.com", "https://acme.com")];
        let e = edge(EdgeKind::Caddy);
        let text = render_site(
            EdgeKind::Caddy,
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
        assert_eq!(
            body(&text),
            "acme.com {
\ttls ops@acme.com
\t@r0 path /api /api/*
\thandle @r0 {
\t\turi strip_prefix /api
\t\treverse_proxy 127.0.0.1:20001
\t}
\thandle {
\t\treverse_proxy 127.0.0.1:20000
\t}
}

admin.acme.com {
\ttls internal
\treverse_proxy 127.0.0.1:20002
}

http://plain.acme.com {
\t@r0 path /v1 /v1/*
\thandle @r0 {
\t\turi strip_prefix /v1
\t\treverse_proxy 127.0.0.1:20003
\t}
\thandle {
\t\trespond 404
\t}
}

www.acme.com {
\ttls ops@acme.com
\tredir https://acme.com{uri} permanent
}

"
        );
    }

    #[test]
    fn renders_static_address_and_snippets() {
        let routes = [
            Route {
                tls: TlsMode::Off,
                ..static_route("site.acme.com", None, true)
            },
            Route {
                tls: TlsMode::Off,
                ..address_route("site.acme.com", Some("/api"), "http://127.0.0.1:3000")
            },
            Route {
                tls: TlsMode::Off,
                snippet: Some("header {\n\tX-A b\n}".into()),
                ..static_route("docs.acme.com", None, false)
            },
        ];
        let e = edge(EdgeKind::Caddy);
        let text = render_site(
            EdgeKind::Caddy,
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
        let www = "/var/lib/ikit/edge/www/acme/w";
        assert_eq!(
            body(&text),
            format!(
                "http://docs.acme.com {{
\troot * {www}/docs.acme.com/abcdef0123456789
\tfile_server
\theader {{
\t\tX-A b
\t}}
}}

http://site.acme.com {{
\t@r0 path /api /api/*
\thandle @r0 {{
\t\turi strip_prefix /api
\t\treverse_proxy http://127.0.0.1:3000
\t}}
\thandle {{
\t\troot * {www}/site.acme.com/abcdef0123456789
\t\ttry_files {{path}} /index.html
\t\tfile_server
\t}}
}}

"
            )
        );
        assert!(text.contains("# ikit serve site.acme.com / static abcdef0123456789\n"));
        assert!(text.contains("# ikit serve site.acme.com /api address http://127.0.0.1:3000\n"));
        assert!(text.contains("# ikit snippet docs.acme.com / "));
    }

    #[test]
    fn staging_ca() {
        let e = EdgeConfig {
            ca: AcmeCa::Staging,
            ..edge(EdgeKind::Caddy)
        };
        assert_eq!(
            tls_directive(TlsMode::Acme, &e).unwrap(),
            "\ttls ops@acme.com {\n\t\tca https://acme-staging-v02.api.letsencrypt.org/directory\n\t}\n"
        );
        let e = EdgeConfig { email: None, ..e };
        assert!(
            tls_directive(TlsMode::Acme, &e)
                .unwrap()
                .starts_with("\ttls {\n")
        );
    }

    #[test]
    fn a_kept_prefix_is_not_stripped() {
        let mut ui = route("dev.acme.dev", Some("/ui/app1"), 20001, TlsMode::Off);
        ui.keep_prefix = true;
        let routes = [ui, route("dev.acme.dev", None, 20002, TlsMode::Off)];
        let input = SiteInput {
            org: "acme",
            workload: "wl-acme-dev-001",
            routes: &routes,
            redirects: &[],
            edge: &edge(EdgeKind::Caddy),
            static_hashes: &hashes(&routes),
        };
        let out = render_site(EdgeKind::Caddy, &input).unwrap();
        assert!(out.contains("reverse_proxy 127.0.0.1:20001"), "{out}");
        assert!(!out.contains("strip_prefix"), "{out}");
        assert!(render_site(EdgeKind::Ferron, &input).is_err());
    }
}
