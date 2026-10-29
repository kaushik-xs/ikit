//! `orgs/<org>/mcp.yaml`: what MCP clients may do in an org. Without the file, MCP is
//! read-only.

use std::fs;

use serde::{Deserialize, Serialize};

use crate::{App, AppError, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// Allow tools that change definitions: create workloads, add services and hooks, set
    /// env, publish ports. They never deploy.
    #[serde(default)]
    pub mutate: bool,
    /// Envs those tools may touch, e.g. `[dev, qa]`. Empty means none.
    #[serde(default)]
    pub mutate_envs: Vec<String>,
    /// Allow `deploy_apply` (of a plan the client was shown) and `deploy_destroy` (with a
    /// confirmation token; volumes are never deleted over MCP).
    #[serde(default)]
    pub deploy: bool,
    /// Envs deploys may touch. Empty means none.
    #[serde(default)]
    pub deploy_envs: Vec<String>,
    /// Allow `zt_authorize`: authorizing and deauthorizing members of the org's ZeroTier
    /// networks.
    #[serde(default)]
    pub zt_authorize: bool,
}

impl McpConfig {
    /// Whether definition changes are allowed for workloads in `env`.
    pub fn allows(&self, env: &str) -> bool {
        self.mutate && self.mutate_envs.iter().any(|e| e == env)
    }

    pub fn allows_deploy(&self, env: &str) -> bool {
        self.deploy && self.deploy_envs.iter().any(|e| e == env)
    }
}

impl App {
    pub fn mcp_config(&self) -> Result<McpConfig> {
        let path = self.org.mcp_config_file();
        if !path.exists() {
            return Ok(McpConfig::default());
        }
        let yaml = fs::read_to_string(&path)?;
        serde_yaml_ng::from_str(&yaml).map_err(|source| AppError::Yaml { path, source })
    }

    /// Fails unless `mcp.yaml` allows deploying to `env`.
    pub fn mcp_check_deploy(&self, env: &str) -> Result<()> {
        if self.mcp_config()?.allows_deploy(env) {
            return Ok(());
        }
        Err(AppError::Invalid(format!(
            "deploying `{env}` workloads over MCP is disabled; to allow it, set `deploy: true` and add `{env}` to \
             `deploy_envs` in {}",
            self.org.mcp_config_file().display()
        )))
    }

    /// Fails unless `mcp.yaml` allows authorizing ZeroTier members.
    pub fn mcp_check_zt_authorize(&self) -> Result<()> {
        if self.mcp_config()?.zt_authorize {
            return Ok(());
        }
        Err(AppError::Invalid(format!(
            "authorizing ZeroTier members over MCP is disabled; to allow it, set `zt_authorize: true` in {}",
            self.org.mcp_config_file().display()
        )))
    }

    /// Fails unless `mcp.yaml` allows changing definitions in `env`.
    pub fn mcp_check_mutate(&self, env: &str) -> Result<()> {
        if self.mcp_config()?.allows(env) {
            return Ok(());
        }
        Err(AppError::Invalid(format!(
            "changing `{env}` workloads over MCP is disabled; to allow it, set `mutate: true` and add `{env}` to \
             `mutate_envs` in {}",
            self.org.mcp_config_file().display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::app;

    #[tokio::test]
    async fn mutate_is_off_until_enabled_per_env() {
        let (_dir, app, _) = app().await;
        assert!(app.mcp_check_mutate("dev").is_err());
        fs::write(
            app.org().mcp_config_file(),
            "mutate: true\nmutate_envs: [dev]\n",
        )
        .unwrap();
        assert!(app.mcp_check_mutate("dev").is_ok());
        assert!(app.mcp_check_mutate("prod").is_err());
        fs::write(
            app.org().mcp_config_file(),
            "mutate: false\nmutate_envs: [dev]\n",
        )
        .unwrap();
        assert!(app.mcp_check_mutate("dev").is_err());
        fs::write(app.org().mcp_config_file(), "mutat: true\n").unwrap();
        assert!(app.mcp_config().is_err(), "typos are rejected");
    }
}
