//! State access for ikit. Callers depend on [`StateStore`] only; SQLite is the one
//! backend today, a Postgres backend for teams can be added later behind the same trait.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::registry::{
    ImportMode, ImportReport, RegistryLogin, ServiceDef, Source, Usage, VersionDef,
};
use ikit_core::target::{Check, TargetOrigin};
use ikit_core::{
    Backup, Category, Connection, Hook, HookAction, HookPhase, HookRunRecord, Network,
    NetworkOrigin, Profile, ProfileKind, PublishedPort, RegistryEntry, RegistryVersion, Run,
    RunAction, RunResult, ServiceInstance, SqlEngine, Storage, StorageOptions, StorageProvider,
    Target, TargetStatus, VersionConstraint, Workload, WorkloadStatus,
};
use semver::Version;

mod sqlite;

pub use sqlite::{OpenReport, SqliteStore};

pub type Result<T, E = StateError> = std::result::Result<T, E>;

pub trait StateStore: Send + Sync {
    fn profiles(&self) -> &dyn ProfileRepo;
    fn registry(&self) -> &dyn RegistryRepo;
    fn targets(&self) -> &dyn TargetRepo;
    fn workloads(&self) -> &dyn WorkloadRepo;
    fn hooks(&self) -> &dyn HookRepo;
    fn storage(&self) -> &dyn StorageRepo;
    fn networks(&self) -> &dyn NetworkRepo;
}

#[async_trait]
pub trait ProfileRepo: Send + Sync {
    /// Fails with [`StateError::Conflict`] if a profile with the same kind and name exists.
    async fn insert(&self, new: NewProfile<'_>) -> Result<Profile>;
    async fn get(&self, kind: ProfileKind, name: &str) -> Result<Option<Profile>>;
    async fn list(&self, kind: Option<ProfileKind>) -> Result<Vec<Profile>>;
    /// Replaces a profile's config and secret keys. Returns `None` if it does not exist.
    async fn update(&self, new: NewProfile<'_>) -> Result<Option<Profile>>;
    /// Returns whether a profile was deleted.
    async fn delete(&self, kind: ProfileKind, name: &str) -> Result<bool>;
}

#[derive(Debug, Clone, Copy)]
pub struct NewProfile<'a> {
    pub kind: ProfileKind,
    pub name: &'a str,
    pub config: &'a BTreeMap<String, String>,
    pub secret_keys: &'a [String],
}

#[async_trait]
pub trait RegistryRepo: Send + Sync {
    /// All services with their versions (newest first), ordered by name.
    async fn list(&self) -> Result<Vec<RegistryEntry>>;
    async fn get(&self, name: &str) -> Result<Option<RegistryEntry>>;
    /// Fails with [`StateError::Conflict`] if the service exists.
    async fn add_service(&self, def: &ServiceDef, source: Source) -> Result<RegistryEntry>;
    /// Fails with `NotFound` for an unknown service and `Conflict` for an existing version.
    async fn add_version(&self, service: &str, def: &VersionDef) -> Result<RegistryVersion>;
    /// Returns whether the version exists.
    async fn set_deprecated(
        &self,
        service: &str,
        version: &Version,
        deprecated: bool,
    ) -> Result<bool>;
    /// Replaces a service's metadata (the versions in `def` are ignored).
    /// Returns whether the service exists.
    async fn update_service(&self, def: &ServiceDef) -> Result<bool>;
    /// Replaces an existing version's image, digest, spec, notes and deprecation.
    /// Returns whether the version exists.
    async fn update_version(&self, service: &str, def: &VersionDef) -> Result<bool>;
    /// Fails with [`StateError::InUse`] while a workload uses the service.
    async fn delete_service(&self, name: &str) -> Result<bool>;
    /// Fails with [`StateError::InUse`] while a workload uses the version.
    async fn delete_version(&self, service: &str, version: &Version) -> Result<bool>;
    /// Applies `defs` in one transaction. Definitions must already be validated.
    async fn import(
        &self,
        defs: &[ServiceDef],
        mode: ImportMode,
        source: Source,
    ) -> Result<ImportReport>;
    /// Workload services using the registry service `name`.
    async fn usage(&self, name: &str) -> Result<Vec<Usage>>;
    async fn logins(&self) -> Result<Vec<RegistryLogin>>;
    /// Adds or replaces the login for `host`.
    async fn set_login(&self, host: &str, username: &str) -> Result<RegistryLogin>;
    async fn delete_login(&self, host: &str) -> Result<bool>;
}

#[async_trait]
pub trait TargetRepo: Send + Sync {
    /// Fails with [`StateError::Conflict`] if the name is taken.
    async fn insert(
        &self,
        name: &str,
        origin: TargetOrigin,
        connection: &Connection,
    ) -> Result<Target>;
    async fn get(&self, name: &str) -> Result<Option<Target>>;
    async fn list(&self) -> Result<Vec<Target>>;
    async fn record_checks(&self, name: &str, status: TargetStatus, checks: &[Check])
    -> Result<()>;
    /// Fails with [`StateError::InUse`] while workloads run on the target.
    async fn delete(&self, name: &str) -> Result<bool>;
}

#[async_trait]
pub trait WorkloadRepo: Send + Sync {
    /// Allocates the next number for `env` and creates `{prefix}-{env}-{nnn}`, atomically.
    async fn create(&self, prefix: &str, env: &str, target_id: i64) -> Result<Workload>;
    async fn get(&self, name: &str) -> Result<Option<Workload>>;
    async fn list(&self, env: Option<&str>) -> Result<Vec<Workload>>;
    /// Deletes the workload with its groups and services.
    async fn delete(&self, name: &str) -> Result<bool>;
    /// Services ordered by group, then name.
    async fn services(&self, workload_id: i64) -> Result<Vec<ServiceInstance>>;
    /// Adds services in one transaction, creating their groups as needed.
    /// Fails with [`StateError::Conflict`] if a name is already used in the workload.
    async fn add_services(&self, workload_id: i64, new: &[NewService<'_>]) -> Result<()>;
    async fn update_service(
        &self,
        workload_id: i64,
        name: &str,
        update: ServiceUpdate<'_>,
    ) -> Result<bool>;
    /// Removes services by name and drops groups left empty. Returns how many were removed.
    async fn remove_services(&self, workload_id: i64, names: &[String]) -> Result<u64>;
    async fn set_status(&self, workload_id: i64, status: WorkloadStatus) -> Result<()>;
    /// Takes the workload's lock for `holder`, or takes over a lock acquired before
    /// `stale_before`. Returns the current holder if someone else has it.
    async fn try_lock(
        &self,
        workload_id: i64,
        holder: &str,
        stale_before: DateTime<Utc>,
    ) -> Result<Option<String>>;
    async fn unlock(&self, workload_id: i64, holder: &str) -> Result<()>;
    async fn start_run(&self, workload_id: i64, run: NewRun<'_>) -> Result<i64>;
    async fn finish_run(&self, run_id: i64, result: RunResult, error: Option<&str>) -> Result<()>;
    /// Most recent first.
    async fn runs(&self, workload_id: i64, limit: i64) -> Result<Vec<Run>>;
}

#[async_trait]
pub trait StorageRepo: Send + Sync {
    /// Fails with `Conflict` on a taken name.
    async fn insert(&self, new: NewStorage<'_>) -> Result<Storage>;
    async fn get(&self, name: &str) -> Result<Option<Storage>>;
    async fn list(&self) -> Result<Vec<Storage>>;
    async fn delete(&self, name: &str) -> Result<bool>;
    async fn add_backup(&self, new: NewBackup<'_>) -> Result<Backup>;
    async fn backup(&self, id: i64) -> Result<Option<Backup>>;
    /// Newest first, optionally for one workload and service.
    async fn backups(&self, workload: Option<&str>, service: Option<&str>) -> Result<Vec<Backup>>;
    async fn delete_backup(&self, id: i64) -> Result<bool>;
}

#[derive(Debug, Clone, Copy)]
pub struct NewStorage<'a> {
    pub name: &'a str,
    pub provider: StorageProvider,
    pub bucket: &'a str,
    pub prefix: &'a str,
    pub profile: Option<&'a str>,
    pub options: &'a StorageOptions,
}

#[derive(Debug, Clone, Copy)]
pub struct NewBackup<'a> {
    pub workload: &'a str,
    pub service: &'a str,
    pub engine: SqlEngine,
    pub storage: &'a str,
    pub object_key: &'a str,
    pub size: i64,
    pub sha256: &'a str,
}

#[async_trait]
pub trait NetworkRepo: Send + Sync {
    /// Fails with `Conflict` on a taken name or network ID.
    async fn insert(&self, new: NewNetwork<'_>) -> Result<Network>;
    async fn get(&self, name: &str) -> Result<Option<Network>>;
    async fn list(&self) -> Result<Vec<Network>>;
    /// Fails with [`StateError::InUse`] while workloads are linked to it.
    async fn delete(&self, name: &str) -> Result<bool>;
    /// Links a workload to a network, replacing any network it was linked to.
    async fn link(&self, workload_id: i64, network_id: i64) -> Result<()>;
    /// Returns whether the workload was linked.
    async fn unlink(&self, workload_id: i64) -> Result<bool>;
    async fn for_workload(&self, workload_id: i64) -> Result<Option<Network>>;
}

#[derive(Debug, Clone, Copy)]
pub struct NewNetwork<'a> {
    pub name: &'a str,
    pub network_id: &'a str,
    pub profile: &'a str,
    pub origin: NetworkOrigin,
    pub cidr: Option<&'a str>,
}

#[async_trait]
pub trait HookRepo: Send + Sync {
    /// Adds a hook after the workload's existing ones. Fails with `Conflict` on a taken name.
    async fn insert(&self, workload_id: i64, new: NewHook<'_>) -> Result<Hook>;
    /// In run order.
    async fn list(&self, workload_id: i64) -> Result<Vec<Hook>>;
    async fn delete(&self, workload_id: i64, name: &str) -> Result<bool>;
    async fn record(&self, hook_id: i64, run: ScriptRun<'_>) -> Result<()>;
    /// Most recent first; `hook` limits it to one hook.
    async fn history(
        &self,
        workload_id: i64,
        hook: Option<&str>,
        limit: i64,
    ) -> Result<Vec<HookRunRecord>>;
}

#[derive(Debug, Clone, Copy)]
pub struct NewHook<'a> {
    pub name: &'a str,
    pub phase: HookPhase,
    pub fail_on_error: bool,
    pub action: &'a HookAction,
}

#[derive(Debug, Clone, Copy)]
pub struct ScriptRun<'a> {
    pub run_id: Option<i64>,
    pub script: &'a str,
    pub checksum: &'a str,
    pub result: RunResult,
    pub error: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewRun<'a> {
    pub action: RunAction,
    pub actor: &'a str,
    pub plan_json: &'a str,
    pub compose_yaml: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewService<'a> {
    pub name: &'a str,
    pub group: Category,
    pub registry_service_id: i64,
    pub version_id: i64,
    pub constraint: &'a VersionConstraint,
    pub env: &'a BTreeMap<String, String>,
    pub secret_env: &'a [String],
}

#[derive(Debug, Clone, Copy)]
pub struct ServiceUpdate<'a> {
    pub version_id: i64,
    pub constraint: &'a VersionConstraint,
    pub env: &'a BTreeMap<String, String>,
    pub secret_env: &'a [String],
    pub publish: &'a [PublishedPort],
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{0} already exists")]
    Conflict(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0} is in use")]
    InUse(String),
    #[error("state database is corrupt: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("migrating the state database failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Maps unique and foreign key violations to domain errors naming `what`.
pub(crate) fn map_constraint(what: impl Into<String>) -> impl FnOnce(sqlx::Error) -> StateError {
    move |e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => StateError::Conflict(what.into()),
        sqlx::Error::Database(db) if is_foreign_key_violation(db.as_ref()) => {
            StateError::InUse(what.into())
        }
        e => e.into(),
    }
}

/// SQLite reports `ON DELETE RESTRICT` failures as SQLITE_CONSTRAINT_TRIGGER (1811) rather than
/// SQLITE_CONSTRAINT_FOREIGNKEY (787), which sqlx's `is_foreign_key_violation` does not cover.
pub(crate) fn is_foreign_key_violation(db: &dyn sqlx::error::DatabaseError) -> bool {
    db.is_foreign_key_violation() || db.message().contains("FOREIGN KEY constraint failed")
}
