use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::{
    HookRepo, NetworkRepo, ProfileRepo, RegistryRepo, Result, StateStore, StorageRepo, TargetRepo,
    WorkloadRepo,
};

mod hooks;
mod networks;
mod profiles;
mod registry;
mod storage;
mod targets;
mod workloads;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations/sqlite");

pub struct SqliteStore {
    profiles: profiles::SqliteProfiles,
    registry: registry::SqliteRegistry,
    targets: targets::SqliteTargets,
    workloads: workloads::SqliteWorkloads,
    hooks: hooks::SqliteHooks,
    storage: storage::SqliteStorage,
    networks: networks::SqliteNetworks,
}

/// What [`SqliteStore::open`] did to the database file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenReport {
    pub created: bool,
    pub applied: Vec<i64>,
    /// Copy taken before migrating an existing database.
    pub backup: Option<PathBuf>,
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path` and applies pending migrations.
    /// An existing database is copied to `<path>.bak-<version>` before it is migrated.
    pub async fn open(path: &Path) -> Result<(Self, OpenReport)> {
        let created = !path.exists();
        let opts = SqliteConnectOptions::from_str("sqlite:")?
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;

        let applied_before = applied_versions(&pool).await?;
        let pending: Vec<i64> = MIGRATOR
            .iter()
            .filter(|m| m.migration_type.is_up_migration() && !applied_before.contains(&m.version))
            .map(|m| m.version)
            .collect();

        let backup = if !created && !pending.is_empty() {
            Some(
                backup(
                    &pool,
                    path,
                    applied_before.iter().max().copied().unwrap_or(0),
                )
                .await?,
            )
        } else {
            None
        };

        MIGRATOR.run(&pool).await?;
        if !pending.is_empty() {
            tracing::info!(?pending, db = %path.display(), "applied state migrations");
        }

        let store = Self {
            profiles: profiles::SqliteProfiles::new(pool.clone()),
            registry: registry::SqliteRegistry::new(pool.clone()),
            targets: targets::SqliteTargets::new(pool.clone()),
            workloads: workloads::SqliteWorkloads::new(pool.clone()),
            hooks: hooks::SqliteHooks::new(pool.clone()),
            storage: storage::SqliteStorage::new(pool.clone()),
            networks: networks::SqliteNetworks::new(pool),
        };
        Ok((
            store,
            OpenReport {
                created,
                applied: pending,
                backup,
            },
        ))
    }
}

impl StateStore for SqliteStore {
    fn profiles(&self) -> &dyn ProfileRepo {
        &self.profiles
    }

    fn registry(&self) -> &dyn RegistryRepo {
        &self.registry
    }

    fn targets(&self) -> &dyn TargetRepo {
        &self.targets
    }

    fn workloads(&self) -> &dyn WorkloadRepo {
        &self.workloads
    }

    fn hooks(&self) -> &dyn HookRepo {
        &self.hooks
    }

    fn storage(&self) -> &dyn StorageRepo {
        &self.storage
    }

    fn networks(&self) -> &dyn NetworkRepo {
        &self.networks
    }
}

async fn applied_versions(pool: &SqlitePool) -> Result<HashSet<i64>> {
    let has_table: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    if !has_table {
        return Ok(HashSet::new());
    }
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = 1")
            .fetch_all(pool)
            .await?;
    Ok(versions.into_iter().collect())
}

/// Takes a consistent snapshot with `VACUUM INTO`, which is safe while other connections are open.
async fn backup(pool: &SqlitePool, path: &Path, version: i64) -> Result<PathBuf> {
    let mut target = PathBuf::from(format!("{}.bak-{version:04}", path.display()));
    if target.exists() {
        target = PathBuf::from(format!(
            "{}.{}",
            target.display(),
            chrono::Utc::now().format("%Y%m%dT%H%M%S")
        ));
    }
    sqlx::query("VACUUM INTO ?")
        .bind(target.to_string_lossy().as_ref())
        .execute(pool)
        .await?;
    tracing::info!(backup = %target.display(), "backed up state database before migrating");
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_and_migrates_new_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        let (_, report) = SqliteStore::open(&path).await.unwrap();
        assert!(report.created);
        assert_eq!(
            report.applied,
            MIGRATOR.iter().map(|m| m.version).collect::<Vec<_>>()
        );
        assert_eq!(report.backup, None);

        let (_, report) = SqliteStore::open(&path).await.unwrap();
        assert_eq!(report, OpenReport::default());
    }

    #[tokio::test]
    async fn backs_up_existing_database_before_migrating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        // An existing file with no migrations applied yet.
        let opts = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        SqlitePool::connect_with(opts).await.unwrap().close().await;

        let (_, report) = SqliteStore::open(&path).await.unwrap();
        assert!(!report.created);
        let backup = report.backup.expect("backup taken");
        assert!(backup.exists());
        assert!(backup.to_string_lossy().ends_with("state.db.bak-0000"));
    }
}
