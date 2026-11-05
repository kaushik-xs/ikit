//! Use cases shared by the CLI and (later) the MCP server. Adapters stay thin: they
//! collect input, call into [`App`], and render the result.

use std::path::PathBuf;
use std::sync::Arc;

use ikit_core::{Config, ProfileError, ProfileKind, RegistryError, VersionError};
use ikit_state::{SqliteStore, StateError, StateStore};

mod age_store;
mod aws_shared;
mod backup;
mod context;
mod deploy;
mod docker_networks;
mod drift;
mod edge;
pub mod exec;
mod hooks;
mod hooks_sql;
mod init;
mod mcp_config;
mod mcp_tokens;
mod org_secrets;
mod orgs;
mod paths;
mod profiles;
mod registry;
mod registry_auth;
mod registry_edit;
mod registry_ops;
mod remote;
mod routes;
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
pub use edge::{
    EdgeInstallAction, EdgeInstallReport, EdgeInstallRequest, EdgePlan, EdgeSite, EdgeStatus,
    EdgeUninstallAction, EdgeUninstallReport, EdgeUpgradeReport,
};
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
pub use org_secrets::{
    IDENTITY_ENV, IdentityOptions, SecretsCheck, SecretsInfo, SecretsInitReport,
    SecretsRecipient, SecretsScope, identity_create, identity_generate, identity_public,
    org_secret_store, org_uses_secrets_file, secrets_check, secrets_grant, secrets_info,
    secrets_merge, secrets_revoke, secrets_split,
};
pub use remote::{
    HistoryEntry, PullReport, Remote, RemoteOverrides, RemoteStatus, RestoreReport, Session,
    SessionOptions, SyncOutcome, org_clone, org_history, org_lock_info, org_pull, org_restore,
    org_status, org_unlock, remote_rm, remote_spec,
};
pub use routes::{RouteList, RouteRequest};
pub use secrets::{KeyringStore, MemorySecretStore, SecretError, SecretStore};
pub use services::{AddServicesReport, ServiceRequest, UnmetDependency};
pub use storage::{NewStorageRequest, ObjectEntry};
pub use targets::ZtSsh;
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

    /// An in-memory ZeroTier Central API v2: one network group, one network ID per create,
    /// members by node ID.
    pub async fn fake_central() -> String {
        use axum::extract::{Path, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use serde_json::{Value, json};

        #[derive(Default)]
        struct Db {
            networks: std::collections::BTreeMap<String, Value>,
            members: std::collections::BTreeMap<(String, String), Value>,
        }
        type Shared = Arc<std::sync::Mutex<Db>>;

        fn authed(h: &HeaderMap) -> Result<(), StatusCode> {
            (h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer secret"))
                .then_some(())
                .ok_or(StatusCode::UNAUTHORIZED)
        }

        fn new_member(node: &str, name: Option<&str>) -> Value {
            json!({ "deviceId": node, "name": name.unwrap_or(""), "status": "not_authorized",
                    "ipv4Assignments": [] })
        }

        fn set_status(
            db: &Shared,
            h: &HeaderMap,
            id: String,
            node: String,
            authorized: bool,
        ) -> Result<Json<Value>, StatusCode> {
            authed(h)?;
            let mut db = db.lock().unwrap();
            let m = db.members.get_mut(&(id, node)).ok_or(StatusCode::NOT_FOUND)?;
            m["status"] = json!(if authorized { "authorized" } else { "not_authorized" });
            m["ipv4Assignments"] = json!(if authorized { vec!["10.147.17.5"] } else { vec![] });
            Ok(Json(m.clone()))
        }

        let app = Router::new()
            .route(
                "/network-group",
                get(|h: HeaderMap| async move {
                    authed(&h)?;
                    Ok::<_, StatusCode>(Json(json!({ "items": [{ "id": "g1", "name": "default" }] })))
                }),
            )
            .route(
                "/network-group/{group}/network",
                post(|State(db): State<Shared>, h: HeaderMap, Json(body): Json<Value>| async move {
                    authed(&h)?;
                    let mut db = db.lock().unwrap();
                    let id = format!("8056c2e21c{:06x}", db.networks.len() + 1);
                    let net = json!({ "id": id, "name": body["name"], "config": body["config"] });
                    db.networks.insert(id, net.clone());
                    Ok::<_, StatusCode>(Json(net))
                }),
            )
            .route(
                "/network/{id}",
                get(|State(db): State<Shared>, h: HeaderMap, Path(id): Path<String>| async move {
                    authed(&h)?;
                    db.lock().unwrap().networks.get(&id).cloned().map(Json).ok_or(StatusCode::NOT_FOUND)
                })
                .delete(|State(db): State<Shared>, h: HeaderMap, Path(id): Path<String>| async move {
                    authed(&h)?;
                    db.lock().unwrap().networks.remove(&id).map(|_| Json(json!({}))).ok_or(StatusCode::NOT_FOUND)
                }),
            )
            .route(
                "/network/{id}/member",
                get(|State(db): State<Shared>, h: HeaderMap, Path(id): Path<String>| async move {
                    authed(&h)?;
                    let db = db.lock().unwrap();
                    let members: Vec<Value> =
                        db.members.iter().filter(|((n, _), _)| *n == id).map(|(_, m)| m.clone()).collect();
                    Ok::<_, StatusCode>(Json(members))
                })
                .post(
                    |State(db): State<Shared>,
                     h: HeaderMap,
                     Path(id): Path<String>,
                     Json(body): Json<Vec<Value>>| async move {
                        authed(&h)?;
                        let mut db = db.lock().unwrap();
                        if !db.networks.contains_key(&id) {
                            return Err(StatusCode::NOT_FOUND);
                        }
                        let mut added = Vec::new();
                        for m in body {
                            let node = m["deviceId"].as_str().unwrap_or_default().to_owned();
                            let member = new_member(&node, m["name"].as_str());
                            db.members.insert((id.clone(), node), member.clone());
                            added.push(member);
                        }
                        Ok(Json(added))
                    },
                ),
            )
            .route(
                "/network/{id}/member/{node}",
                get(
                    |State(db): State<Shared>,
                     h: HeaderMap,
                     Path((id, node)): Path<(String, String)>| async move {
                        authed(&h)?;
                        db.lock().unwrap().members.get(&(id, node)).cloned().map(Json).ok_or(StatusCode::NOT_FOUND)
                    },
                )
                .post(
                    |State(db): State<Shared>,
                     h: HeaderMap,
                     Path((id, node)): Path<(String, String)>,
                     Json(body): Json<Value>| async move {
                        authed(&h)?;
                        let mut db = db.lock().unwrap();
                        let m = db.members.get_mut(&(id, node)).ok_or(StatusCode::NOT_FOUND)?;
                        if let Some(n) = body["name"].as_str() {
                            m["name"] = json!(n);
                        }
                        Ok::<_, StatusCode>(Json(m.clone()))
                    },
                ),
            )
            .route(
                "/network/{id}/member/{node}/authorize",
                post(
                    |State(db): State<Shared>,
                     h: HeaderMap,
                     Path((id, node)): Path<(String, String)>| async move {
                        set_status(&db, &h, id, node, true)
                    },
                ),
            )
            .route(
                "/network/{id}/member/{node}/de-authorize",
                post(
                    |State(db): State<Shared>,
                     h: HeaderMap,
                     Path((id, node)): Path<(String, String)>| async move {
                        set_status(&db, &h, id, node, false)
                    },
                ),
            )
        .with_state(Shared::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    /// A zerotier profile `zt` pointing at a [`fake_central`] that accepts token `secret`.
    pub async fn zt_profile(app: &App) {
        let url = fake_central().await;
        app.profile_add(crate::AddProfile {
            kind: ikit_core::ProfileKind::Zerotier,
            name: "zt".into(),
            values: [("api_url", url.as_str()), ("api_token", "secret"), ("org_id", "org1")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        })
        .await
        .unwrap();
    }
}
