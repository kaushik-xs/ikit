//! Ferron 3 host blocks (`ferron.conf` syntax). A host block listens on 80 and 443 with
//! automatic TLS and redirects HTTP to HTTPS; `tls false` with `https_redirect false`
//! serves plain HTTP only. (`host:80` would serve TLS on port 80.) `location` blocks match
//! path prefixes and strip them before proxying.

use ikit_core::{EdgeConfig, Redirect, TlsMode};

use super::HostRoutes;

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

pub(super) fn render(
    hosts: &[HostRoutes<'_>],
    redirects: &[Redirect],
    edge: &EdgeConfig,
) -> String {
    let mut out = String::new();
    for h in hosts {
        out.push_str(&format!("{} {{\n", h.host));
        out.push_str(&tls_block(h.tls, edge));
        for r in &h.routes {
            match &r.path {
                Some(path) => out.push_str(&format!(
                    "    location {path} {{\n        proxy http://127.0.0.1:{}\n    }}\n",
                    r.upstream_port
                )),
                None => out.push_str(&format!("    proxy http://127.0.0.1:{}\n", r.upstream_port)),
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

    use super::super::tests::{edge, redirect, route};
    use super::super::{SiteInput, render_site};
    use super::*;

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
    fn production_without_email_needs_no_tls_block() {
        let e = EdgeConfig {
            email: None,
            ..edge(EdgeKind::Ferron)
        };
        assert_eq!(tls_block(TlsMode::Acme, &e), "");
    }
}
