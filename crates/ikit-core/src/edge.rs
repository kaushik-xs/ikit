//! The edge: a reverse proxy (Caddy or Ferron) installed on a VM target's host, outside
//! Docker, that owns ports 80 and 443 and routes domains to workload services published
//! on the host's loopback address, with certificates from ACME.
//!
//! One proxy serves every workload on a host, including workloads of other orgs. The host
//! is the source of truth for who uses it: `host.json` records the proxy and the orgs that
//! joined it, and each workload's routes live in their own site file whose header names
//! the org, the workload and the routes, so any org can see what the others claimed.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::registry::Category;

string_enum! {
    "edge server",
    pub enum EdgeKind {
        Caddy => "caddy",
        Ferron => "ferron",
    }
}

string_enum! {
    "ACME CA",
    pub enum AcmeCa {
        /// Let's Encrypt production: trusted certificates, strict rate limits.
        Production => "production",
        /// Let's Encrypt staging: untrusted certificates, for trying a setup out.
        Staging => "staging",
    }
}

impl AcmeCa {
    /// The ACME directory URL, or `None` for the server's default (Let's Encrypt production).
    pub fn directory(self) -> Option<&'static str> {
        match self {
            Self::Production => None,
            Self::Staging => Some("https://acme-staging-v02.api.letsencrypt.org/directory"),
        }
    }
}

string_enum! {
    "TLS mode",
    pub enum TlsMode {
        /// A public certificate from ACME; the domain must resolve to the host and ports
        /// 80/443 must be reachable from the internet.
        Acme => "acme",
        /// A certificate from the proxy's own local CA, for private hosts (ZeroTier only).
        Internal => "internal",
        /// Plain HTTP.
        Off => "off",
    }
}

string_enum! {
    "route origin",
    pub enum RouteOrigin {
        /// Added by `ikit route auto` from the org's host template.
        Auto => "auto",
        /// Added with `ikit route add`.
        Manual => "manual",
    }
}

/// How this org uses a target's edge; stored on the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeConfig {
    pub kind: EdgeKind,
    /// ACME account email for this org's certificates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default = "default_ca")]
    pub ca: AcmeCa,
    /// The proxy's version when this org last installed, joined or upgraded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

fn default_ca() -> AcmeCa {
    AcmeCa::Production
}

/// `/etc/ikit/edge/host.json` on the target: the proxy and the orgs that use it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostManifest {
    pub kind: EdgeKind,
    /// The org that installed or adopted the proxy.
    pub installed_by: String,
    /// The proxy was there before ikit: ikit only added its include line to the main
    /// config, and never removes the package or stops the service.
    #[serde(default)]
    pub adopted: bool,
    /// Only `installed_by` may use the proxy.
    #[serde(default)]
    pub exclusive: bool,
    /// Orgs that joined, `installed_by` first.
    pub orgs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub installed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgraded_at: Option<DateTime<Utc>>,
}

/// A domain (and optional path prefix) routed to a service port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Route {
    pub id: i64,
    pub service: String,
    /// The container port traffic goes to.
    pub port: u16,
    pub host: String,
    /// A path prefix such as `/api`, stripped before proxying; `None` for the whole host.
    pub path: Option<String>,
    pub tls: TlsMode,
    /// The loopback port on the host that the container port is published on.
    pub upstream_port: u16,
    pub origin: RouteOrigin,
    pub created_at: DateTime<Utc>,
}

impl Route {
    /// `host` or `host/path`, how routes are named on the command line.
    pub fn key(&self) -> String {
        route_key(&self.host, self.path.as_deref())
    }

    /// The URL the route answers on.
    pub fn url(&self) -> String {
        let scheme = if self.tls == TlsMode::Off {
            "http"
        } else {
            "https"
        };
        format!(
            "{scheme}://{}{}",
            self.host,
            self.path.as_deref().unwrap_or("")
        )
    }
}

pub fn route_key(host: &str, path: Option<&str>) -> String {
    format!("{host}{}", path.unwrap_or(""))
}

/// A domain that redirects to a URL, keeping the request path (`www` → apex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Redirect {
    pub id: i64,
    pub host: String,
    /// `https://example.com`; the request URI is appended.
    pub to: String,
    pub permanent: bool,
    pub tls: TlsMode,
    pub created_at: DateTime<Utc>,
}

/// Routes ikit keeps in step with a workload's services: one per service in `groups` with
/// a registry port named `port_name`, at the org's host template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoRoutes {
    pub groups: Vec<Category>,
    pub port_name: String,
    /// Services whose auto route was removed by hand; they do not get one back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<String>,
}

/// Whether `host` is a lowercase DNS name with at least two labels. Wildcards are refused:
/// they need DNS-01 challenges.
pub fn is_hostname(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    (1..=253).contains(&host.len())
        && labels.len() >= 2
        && labels.iter().all(|l| {
            (1..=63).contains(&l.len())
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// Normalizes a path prefix: `api/` → `/api`, `/` → none. Only URL-safe path characters
/// are allowed, so the path can go into proxy configs unquoted.
pub fn parse_route_path(path: &str) -> Result<Option<String>, String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(None);
    }
    let ok = trimmed.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
    });
    if !ok || trimmed.len() > 200 {
        return Err(format!(
            "invalid path `{path}`: use segments of letters, digits, `-`, `.`, `_` or `~`, e.g. /api/v1"
        ));
    }
    Ok(Some(format!("/{trimmed}")))
}

/// Checks a redirect target: an http(s) URL without a query, fragment, whitespace,
/// quotes or braces (it is written into proxy configs).
pub fn parse_redirect_target(to: &str) -> Result<String, String> {
    let to = to.trim_end_matches('/');
    let rest = to
        .strip_prefix("https://")
        .or_else(|| to.strip_prefix("http://"));
    let host = rest.map(|r| r.split('/').next().unwrap_or(""));
    let clean = !to
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || "\"'{}\\?#`$<>".contains(c));
    match host {
        Some(h) if clean && (is_hostname(h) || is_host_port(h)) => Ok(to.to_owned()),
        _ => Err(format!(
            "invalid redirect target `{to}`: use an http(s) URL such as https://example.com"
        )),
    }
}

fn is_host_port(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((h, p)) => is_hostname(h) && p.parse::<u16>().is_ok(),
        None => false,
    }
}

/// A plausible email address that is safe to write into proxy configs.
pub fn is_email(email: &str) -> bool {
    match email.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && local.len() <= 64
                && local
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c))
                && is_hostname(&domain.to_ascii_lowercase())
        }
        None => false,
    }
}

/// Fills `{service}`, `{workload}`, `{env}`, `{org}` and `{base_domain}` in a host
/// template and checks the result is a hostname.
pub fn expand_host_template(template: &str, vars: &[(&str, &str)]) -> Result<String, String> {
    let mut host = template.to_owned();
    for (k, v) in vars {
        host = host.replace(&format!("{{{k}}}"), v);
    }
    if host.contains('{') || !is_hostname(&host) {
        return Err(format!(
            "host template `{template}` gives `{host}`, which is not a hostname \
             (placeholders: {{service}}, {{workload}}, {{env}}, {{org}}, {{base_domain}})"
        ));
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames() {
        assert!(is_hostname("api.acme.com"));
        assert!(is_hostname("web.wl-acme-dev-001.acme.dev"));
        assert!(!is_hostname("localhost"));
        assert!(!is_hostname("*.acme.com"));
        assert!(!is_hostname("Api.acme.com"));
        assert!(!is_hostname("-a.acme.com"));
        assert!(!is_hostname("a..com"));
        assert!(!is_hostname("a.com\n"));
    }

    #[test]
    fn paths_are_normalized() {
        assert_eq!(parse_route_path("/"), Ok(None));
        assert_eq!(parse_route_path("api/"), Ok(Some("/api".into())));
        assert_eq!(parse_route_path("/api/v1"), Ok(Some("/api/v1".into())));
        assert!(parse_route_path("/a/../b").is_err());
        assert!(parse_route_path("/a b").is_err());
        assert!(parse_route_path("/a}").is_err());
    }

    #[test]
    fn redirect_targets() {
        assert_eq!(
            parse_redirect_target("https://acme.com/"),
            Ok("https://acme.com".into())
        );
        assert!(parse_redirect_target("https://acme.com:8443/app").is_ok());
        assert!(parse_redirect_target("ftp://acme.com").is_err());
        assert!(parse_redirect_target("https://acme.com/{x}").is_err());
        assert!(parse_redirect_target("https://acme.com/?q=1").is_err());
    }

    #[test]
    fn emails() {
        assert!(is_email("ops@acme.com"));
        assert!(!is_email("ops"));
        assert!(!is_email("o ps@acme.com"));
        assert!(!is_email("ops@acme.com}"));
    }

    #[test]
    fn templates() {
        let vars = [
            ("service", "web"),
            ("workload", "wl-acme-dev-001"),
            ("base_domain", "acme.dev"),
        ];
        assert_eq!(
            expand_host_template("{service}.{workload}.{base_domain}", &vars),
            Ok("web.wl-acme-dev-001.acme.dev".into())
        );
        assert!(expand_host_template("{service}.{nope}.dev", &vars).is_err());
    }

    #[test]
    fn urls() {
        let r = Route {
            id: 1,
            service: "api".into(),
            port: 8080,
            host: "api.acme.com".into(),
            path: Some("/v1".into()),
            tls: TlsMode::Acme,
            upstream_port: 20000,
            origin: RouteOrigin::Manual,
            created_at: Utc::now(),
        };
        assert_eq!(r.url(), "https://api.acme.com/v1");
        assert_eq!(r.key(), "api.acme.com/v1");
    }
}
