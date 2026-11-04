use serde::{Deserialize, Serialize};

use crate::edge::{TlsMode, is_email, is_hostname};
use crate::registry::Category;
use crate::workload::is_env;

/// Contents of an org's `config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Workload names are `{prefix}-{env}-{nnn}`. Homes created before orgs existed keep `wl`.
    #[serde(default = "legacy_prefix")]
    pub prefix: String,
    /// Environments a workload can be created in.
    pub envs: Vec<String>,
    /// Envs whose deploys must use digest-pinned images (`ikit registry version pin`).
    #[serde(default = "default_digest_envs")]
    pub require_digest_envs: Vec<String>,
    /// Defaults for routes through a target's reverse proxy.
    #[serde(default, skip_serializing_if = "EdgeDefaults::is_default")]
    pub edge: EdgeDefaults,
}

/// The `edge:` section of `config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EdgeDefaults {
    /// Domain that routes without an explicit host go under, e.g. `acme.dev`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_domain: Option<String>,
    /// ACME account email for this org's certificates, on every target's proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acme_email: Option<String>,
    /// Host of a route without an explicit one: `{service}`, `{workload}`, `{env}`,
    /// `{org}` and `{base_domain}` are filled in.
    pub host_template: String,
    pub default_tls: TlsMode,
    /// Envs whose routes may not use `tls: off`.
    pub require_tls_envs: Vec<String>,
    /// Loopback host ports handed out to routed services, `[first, last]`.
    pub upstream_ports: [u16; 2],
    /// Turn on automatic routes for new workloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_routes: Option<AutoRouteDefaults>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoRouteDefaults {
    #[serde(default = "default_auto_groups")]
    pub groups: Vec<Category>,
    #[serde(default = "default_port_name")]
    pub port_name: String,
}

fn default_auto_groups() -> Vec<Category> {
    vec![Category::App]
}

pub fn default_port_name() -> String {
    "http".to_owned()
}

impl Default for EdgeDefaults {
    fn default() -> Self {
        Self {
            base_domain: None,
            acme_email: None,
            host_template: "{service}.{workload}.{base_domain}".to_owned(),
            default_tls: TlsMode::Acme,
            require_tls_envs: default_digest_envs(),
            upstream_ports: [20000, 29999],
            auto_routes: None,
        }
    }
}

impl EdgeDefaults {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn default_digest_envs() -> Vec<String> {
    vec!["prod".to_owned()]
}

fn legacy_prefix() -> String {
    "wl".to_owned()
}

pub const DEFAULT_ENVS: [&str; 4] = ["dev", "qa", "stg", "prod"];

impl Default for Config {
    fn default() -> Self {
        Self {
            display_name: None,
            prefix: legacy_prefix(),
            envs: DEFAULT_ENVS.map(String::from).to_vec(),
            require_digest_envs: default_digest_envs(),
            edge: EdgeDefaults::default(),
        }
    }
}

impl Config {
    /// Config for a new org: prefix `wl-<org>` so the org shows in every workload name.
    pub fn for_org(org: &str) -> Self {
        Self {
            prefix: format!("wl-{org}"),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !crate::is_slug(&self.prefix) || self.prefix.len() > 32 {
            return Err(format!(
                "invalid prefix `{}`: use up to 32 characters of {}",
                self.prefix,
                crate::SLUG_RULE
            ));
        }
        if self.envs.is_empty() {
            return Err("at least one env is required".to_owned());
        }
        for (i, env) in self.envs.iter().enumerate() {
            if !is_env(env) {
                return Err(format!(
                    "invalid env `{env}`: use 1-16 lowercase letters or digits, starting with a letter"
                ));
            }
            if self.envs[..i].contains(env) {
                return Err(format!("env `{env}` is listed twice"));
            }
        }
        if let Some(d) = &self.edge.base_domain
            && !is_hostname(d)
        {
            return Err(format!(
                "invalid edge.base_domain `{d}`: use a domain such as acme.dev"
            ));
        }
        if let Some(e) = &self.edge.acme_email
            && !is_email(e)
        {
            return Err(format!(
                "invalid edge.acme_email `{e}`: use an address such as ops@acme.com"
            ));
        }
        let [first, last] = self.edge.upstream_ports;
        if first < 1024 || first > last {
            return Err(format!(
                "invalid edge.upstream_ports [{first}, {last}]: use a range above 1023, first <= last"
            ));
        }
        if self
            .edge
            .auto_routes
            .as_ref()
            .is_some_and(|a| a.groups.is_empty())
        {
            return Err("edge.auto_routes.groups needs at least one group".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_config_uses_org_in_prefix() {
        let c = Config::for_org("acme");
        assert_eq!(c.prefix, "wl-acme");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn old_config_files_get_the_legacy_prefix() {
        let c: Config = serde_yaml_ng::from_str("envs: [dev, prod]").unwrap();
        assert_eq!(c.prefix, "wl");
    }

    #[test]
    fn validates_prefix_and_envs() {
        let bad = |f: fn(&mut Config)| {
            let mut c = Config::default();
            f(&mut c);
            c.validate().is_err()
        };
        assert!(bad(|c| c.prefix = "Acme".into()));
        assert!(bad(|c| c.prefix = "a".repeat(33)));
        assert!(bad(|c| c.envs.clear()));
        assert!(bad(|c| c.envs.push("dev".into())));
        assert!(bad(|c| c.envs.push("u-a-t".into())));
        assert!(bad(|c| c.edge.base_domain = Some("dev".into())));
        assert!(bad(|c| c.edge.upstream_ports = [80, 90]));
        assert!(bad(|c| c.edge.acme_email = Some("ops".into())));
    }

    #[test]
    fn edge_section_is_optional() {
        let c: Config = serde_yaml_ng::from_str(
            "envs: [dev]\nedge:\n  base_domain: acme.dev\n  auto_routes: {}\n",
        )
        .unwrap();
        assert_eq!(c.edge.base_domain.as_deref(), Some("acme.dev"));
        assert_eq!(c.edge.host_template, "{service}.{workload}.{base_domain}");
        assert_eq!(c.edge.auto_routes.unwrap().groups, vec![Category::App]);
        assert!(
            !serde_yaml_ng::to_string(&Config::default())
                .unwrap()
                .contains("edge")
        );
    }
}
