//! Caddyfile site blocks. Caddy redirects HTTP to HTTPS by itself and gets certificates
//! for every site address that is a public domain.

use ikit_core::{EdgeConfig, Redirect, TlsMode};

use super::HostRoutes;

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
            out.push_str(&format!(
                "\treverse_proxy 127.0.0.1:{}\n",
                h.routes[0].upstream_port
            ));
        } else {
            for (i, r) in h.routes.iter().enumerate() {
                match &r.path {
                    Some(path) => out.push_str(&format!(
                        "\t@r{i} path {path} {path}/*\n\
                         \thandle @r{i} {{\n\
                         \t\turi strip_prefix {path}\n\
                         \t\treverse_proxy 127.0.0.1:{}\n\
                         \t}}\n",
                        r.upstream_port
                    )),
                    None => out.push_str(&format!(
                        "\thandle {{\n\t\treverse_proxy 127.0.0.1:{}\n\t}}\n",
                        r.upstream_port
                    )),
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

    use super::super::tests::{edge, redirect, route};
    use super::super::{SiteInput, render_site};
    use super::*;

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
}
