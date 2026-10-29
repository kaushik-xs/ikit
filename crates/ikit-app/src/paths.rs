use std::path::PathBuf;

use crate::{AppError, Result};

/// The ikit home, `$IKIT_HOME` or `~/.ikit`. Each org has its own folder under `orgs/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// Uses `home` when given (e.g. from `--home` / `IKIT_HOME`), otherwise `~/.ikit`.
    pub fn resolve(home: Option<PathBuf>) -> Result<Self> {
        match home {
            Some(home) => Ok(Self::new(home)),
            None => dirs::home_dir()
                .map(|h| Self::new(h.join(".ikit")))
                .ok_or(AppError::NoHome),
        }
    }

    /// The active org and each org's active workload.
    pub fn context_file(&self) -> PathBuf {
        self.home.join("context.yaml")
    }

    pub fn orgs_dir(&self) -> PathBuf {
        self.home.join("orgs")
    }

    pub fn org(&self, name: &str) -> OrgPaths {
        OrgPaths {
            name: name.to_owned(),
            dir: self.orgs_dir().join(name),
        }
    }

    /// The single state database of homes created before orgs existed.
    pub(crate) fn legacy_state_db(&self) -> PathBuf {
        self.home.join("state.db")
    }
}

/// One org's folder: its own state database, config and workload specs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgPaths {
    pub name: String,
    pub dir: PathBuf,
}

impl OrgPaths {
    pub fn state_db(&self) -> PathBuf {
        self.dir.join("state.db")
    }

    pub fn config_file(&self) -> PathBuf {
        self.dir.join("config.yaml")
    }

    /// What MCP clients may do in this org.
    pub fn mcp_config_file(&self) -> PathBuf {
        self.dir.join("mcp.yaml")
    }

    /// Hashes of the bearer tokens `ikit mcp serve --http` accepts.
    pub fn mcp_tokens_file(&self) -> PathBuf {
        self.dir.join("mcp-tokens.yaml")
    }

    /// Generated spec for a workload, `workloads/<name>.yaml`.
    pub fn workload_spec(&self, name: &str) -> PathBuf {
        self.dir.join("workloads").join(format!("{name}.yaml"))
    }
}
