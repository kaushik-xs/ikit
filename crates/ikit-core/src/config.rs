use serde::{Deserialize, Serialize};

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
    }
}
