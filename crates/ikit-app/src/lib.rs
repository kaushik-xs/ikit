//! Use cases shared by the CLI and (later) the MCP server. Adapters stay thin: they
//! collect input, call into [`App`], and render the result.

use std::path::PathBuf;
use std::sync::Arc;

use ikit_core::{Config, ProfileError, ProfileKind, RegistryError, VersionError};
use ikit_state::{SqliteStore, StateError, StateStore};

mod backup;
mod context;
mod deploy;
mod drift;
pub mod exec;
mod hooks;
mod hooks_sql;
mod init;
mod mcp_config;
mod mcp_tokens;
mod orgs;
mod paths;
mod profiles;
mod registry;
mod registry_auth;
mod registry_edit;
mod registry_ops;
pub mod secrets;
mod services;
mod storage;
mod targets;
mod workloads;
mod zerotier;

pub use backup::BackupReport;
pub use context::resolve_org;
pub use deploy::{ApplyReport, ContainerStatus, DeployPlan};
pub use drift::{DriftItem, DriftKind, DriftReport};
pub use exec::{Exec, ExecOutput, SystemExec};
pub use hooks::{HookOutcome, HookPlan, HookRequest, NewHookRequest};
pub use init::{InitReport, OrgInit, init};
pub use mcp_config::McpConfig;
pub use mcp_tokens::{McpToken, mcp_token_verify};
pub use orgs::{
    NewOrg, OrgSummary, find_org_for_workload, org_config, org_create, org_list, org_remove,
    org_show, org_use,
};
pub use paths::{OrgPaths, Paths};
pub use profiles::AddProfile;
pub use registry::{AddVersion, Outdated, RegistryFilter};
pub use registry_edit::{ServiceChanges, VersionChanges, VersionUpdate};
pub use registry_ops::{ImageRef, SyncReport};
pub use secrets::{KeyringStore, MemorySecretStore, SecretError, SecretStore};
pub use services::{AddServicesReport, ServiceRequest, UnmetDependency};
pub use storage::{NewStorageRequest, ObjectEntry};
pub use workloads::{SPEC_HEADER, WorkloadDetail};
pub use zerotier::{ZtJoin, ZtNetworkDetail, ZtNode};

pub type Result<T, E = AppError> = std::result::Result<T, E>;

/// Use cases for one org: its own state database, config and keychain namespace.
pub struct App {
    paths: Paths,
    org: OrgPaths,
    store: Arc<dyn StateStore>,
    secrets: Arc<dyn SecretStore>,
    exec: Arc<dyn Exec>,
}

impl App {
    /// Opens `org`'s state database, applying any pending migrations.
    pub async fn open(paths: &Paths, org: &str, secrets: Arc<dyn SecretStore>) -> Result<Self> {
        let org_paths = paths.org(org);
        let db = org_paths.state_db();
        if !db.exists() {
            if paths.legacy_state_db().exists() {
                return Err(AppError::Invalid(
                    "this ikit home predates orgs; run `ikit init` to move it into the `default` org".to_owned(),
                ));
            }
            return Err(AppError::NotFound(format!(
                "org `{org}` (see `ikit org list`)"
            )));
        }
        let (store, _) = SqliteStore::open(&db).await?;
        Ok(Self::new(
            paths.clone(),
            org_paths,
            Arc::new(store),
            secrets,
        ))
    }

    pub fn new(
        paths: Paths,
        org: OrgPaths,
        store: Arc<dyn StateStore>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        Self {
            paths,
            org,
            store,
            secrets,
            exec: Arc::new(SystemExec),
        }
    }

    /// Replaces how external commands (ssh, docker, kubectl) are run; used by tests.
    pub fn with_exec(mut self, exec: Arc<dyn Exec>) -> Self {
        self.exec = exec;
        self
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// This org's name and folder.
    pub fn org(&self) -> &OrgPaths {
        &self.org
    }

    /// This org's `config.yaml`.
    pub fn config(&self) -> Result<Config> {
        org_config(&self.paths, &self.org.name)
    }

    /// Keychain account holding a profile's secret fields as one JSON object.
    pub(crate) fn profile_secret_key(&self, kind: ProfileKind, name: &str) -> String {
        format!("org/{}/profile/{kind}/{name}", self.org.name)
    }

    /// Keychain account holding a workload service's secret env values as one JSON object.
    pub(crate) fn service_secret_key(&self, workload: &str, service: &str) -> String {
        format!("org/{}/workload/{workload}/{service}", self.org.name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("ikit is not initialized in {} (run `ikit init`)", .0.display())]
    NotInitialized(PathBuf),
    #[error(
        "profile {kind}/{name} already exists (remove it first with `ikit profile rm {kind} {name}`)"
    )]
    ProfileExists { kind: ProfileKind, name: String },
    #[error("profile {kind}/{name} not found")]
    ProfileNotFound { kind: ProfileKind, name: String },
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0} already exists")]
    AlreadyExists(String),
    #[error("{what} is in use{}", in_use_by(.by))]
    InUse { what: String, by: Vec<String> },
    #[error("{0}")]
    Invalid(String),
    #[error("no workload selected: pass --wl <name> or run `ikit wl use <name>`")]
    NoWorkloadSelected,
    #[error(
        "no org selected: pass --org <name> or run `ikit org use <name>` (create one with `ikit org create <name>`)"
    )]
    NoOrgSelected,
    #[error("cannot determine the home directory; set IKIT_HOME")]
    NoHome,
    #[error("invalid config file {}: {source}", path.display())]
    Config {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
    #[error("invalid YAML in {}: {source}", path.display())]
    Yaml {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Version(#[from] VersionError),
    #[error(transparent)]
    State(StateError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn in_use_by(by: &[String]) -> String {
    if by.is_empty() {
        String::new()
    } else {
        format!(" by {}", by.join(", "))
    }
}

impl From<StateError> for AppError {
    fn from(e: StateError) -> Self {
        match e {
            StateError::Conflict(what) => Self::AlreadyExists(what),
            StateError::NotFound(what) => Self::NotFound(what),
            StateError::InUse(what) => Self::InUse { what, by: vec![] },
            e => Self::State(e),
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use super::*;

    /// An app for org `acme` (prefix `wl-acme`) in a temp home, with in-memory secrets.
    pub async fn app() -> (tempfile::TempDir, App, Arc<MemorySecretStore>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let secrets = Arc::new(MemorySecretStore::default());
        org_create(
            &paths,
            NewOrg {
                name: "acme".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let app = App::open(&paths, "acme", secrets.clone()).await.unwrap();
        (dir, app, secrets)
    }
}
