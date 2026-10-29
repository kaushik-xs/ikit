use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::{Backup, Storage, StorageOptions, UnknownVariant};
use sqlx::SqlitePool;
use sqlx::types::Json;

use crate::{NewBackup, NewStorage, Result, StateError, StorageRepo, map_constraint};

pub(super) struct SqliteStorage {
    pool: SqlitePool,
}

impl SqliteStorage {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

struct StorageRow {
    id: i64,
    name: String,
    provider: String,
    bucket: String,
    prefix: String,
    profile: Option<String>,
    options_json: Json<StorageOptions>,
    created_at: DateTime<Utc>,
}

impl TryFrom<StorageRow> for Storage {
    type Error = StateError;

    fn try_from(r: StorageRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            provider: r
                .provider
                .parse()
                .map_err(|e: UnknownVariant| StateError::Corrupt(e.to_string()))?,
            name: r.name,
            bucket: r.bucket,
            prefix: r.prefix,
            profile: r.profile,
            options: r.options_json.0,
            created_at: r.created_at,
        })
    }
}

struct BackupRow {
    id: i64,
    workload: String,
    service: String,
    engine: String,
    storage: String,
    object_key: String,
    size: i64,
    sha256: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<BackupRow> for Backup {
    type Error = StateError;

    fn try_from(r: BackupRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            engine: r
                .engine
                .parse()
                .map_err(|e: UnknownVariant| StateError::Corrupt(e.to_string()))?,
            workload: r.workload,
            service: r.service,
            storage: r.storage,
            object_key: r.object_key,
            size: r.size,
            sha256: r.sha256,
            created_at: r.created_at,
        })
    }
}

#[async_trait]
impl StorageRepo for SqliteStorage {
    async fn insert(&self, new: NewStorage<'_>) -> Result<Storage> {
        let provider = new.provider.as_str();
        let options = Json(new.options);
        let now = Utc::now();
        sqlx::query_as!(
            StorageRow,
            r#"INSERT INTO storages (name, provider, bucket, prefix, profile, options_json, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!", name, provider, bucket, prefix, profile,
                         options_json AS "options_json: Json<StorageOptions>", created_at AS "created_at: DateTime<Utc>""#,
            new.name,
            provider,
            new.bucket,
            new.prefix,
            new.profile,
            options,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_constraint(format!("storage {}", new.name)))?
        .try_into()
    }

    async fn get(&self, name: &str) -> Result<Option<Storage>> {
        sqlx::query_as!(
            StorageRow,
            r#"SELECT id AS "id!", name, provider, bucket, prefix, profile,
                      options_json AS "options_json: Json<StorageOptions>", created_at AS "created_at: DateTime<Utc>"
               FROM storages WHERE name = ?"#,
            name,
        )
        .fetch_optional(&self.pool)
        .await?
        .map(Storage::try_from)
        .transpose()
    }

    async fn list(&self) -> Result<Vec<Storage>> {
        sqlx::query_as!(
            StorageRow,
            r#"SELECT id AS "id!", name, provider, bucket, prefix, profile,
                      options_json AS "options_json: Json<StorageOptions>", created_at AS "created_at: DateTime<Utc>"
               FROM storages ORDER BY name"#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Storage::try_from)
        .collect()
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM storages WHERE name = ?", name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn add_backup(&self, new: NewBackup<'_>) -> Result<Backup> {
        let engine = new.engine.as_str();
        let now = Utc::now();
        sqlx::query_as!(
            BackupRow,
            r#"INSERT INTO backups (workload, service, engine, storage, object_key, size, sha256, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!", workload, service, engine, storage, object_key, size, sha256,
                         created_at AS "created_at: DateTime<Utc>""#,
            new.workload,
            new.service,
            engine,
            new.storage,
            new.object_key,
            new.size,
            new.sha256,
            now,
        )
        .fetch_one(&self.pool)
        .await?
        .try_into()
    }

    async fn backup(&self, id: i64) -> Result<Option<Backup>> {
        sqlx::query_as!(
            BackupRow,
            r#"SELECT id AS "id!", workload, service, engine, storage, object_key, size, sha256,
                      created_at AS "created_at: DateTime<Utc>"
               FROM backups WHERE id = ?"#,
            id,
        )
        .fetch_optional(&self.pool)
        .await?
        .map(Backup::try_from)
        .transpose()
    }

    async fn backups(&self, workload: Option<&str>, service: Option<&str>) -> Result<Vec<Backup>> {
        sqlx::query_as!(
            BackupRow,
            r#"SELECT id AS "id!", workload, service, engine, storage, object_key, size, sha256,
                      created_at AS "created_at: DateTime<Utc>"
               FROM backups
               WHERE (?1 IS NULL OR workload = ?1) AND (?2 IS NULL OR service = ?2)
               ORDER BY id DESC"#,
            workload,
            service,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Backup::try_from)
        .collect()
    }

    async fn delete_backup(&self, id: i64) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM backups WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use ikit_core::{SqlEngine, StorageProvider};

    use super::*;
    use crate::{SqliteStore, StateStore};

    #[tokio::test]
    async fn storages_and_backups() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = SqliteStore::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        let options = StorageOptions {
            endpoint: Some("http://localhost:9000".into()),
            ..Default::default()
        };
        let new = NewStorage {
            name: "minio",
            provider: StorageProvider::S3,
            bucket: "backups",
            prefix: "ikit",
            profile: Some("local"),
            options: &options,
        };
        let s = store.storage().insert(new).await.unwrap();
        assert_eq!(s.options, options);
        assert!(matches!(
            store.storage().insert(new).await,
            Err(StateError::Conflict(_))
        ));

        let backup = |service| NewBackup {
            workload: "wl-dev-001",
            service,
            engine: SqlEngine::Postgres,
            storage: "minio",
            object_key: "k",
            size: 10,
            sha256: "abc",
        };
        store
            .storage()
            .add_backup(backup("postgres"))
            .await
            .unwrap();
        let b2 = store
            .storage()
            .add_backup(backup("postgres"))
            .await
            .unwrap();
        store.storage().add_backup(backup("mysql")).await.unwrap();
        let pg = store
            .storage()
            .backups(Some("wl-dev-001"), Some("postgres"))
            .await
            .unwrap();
        assert_eq!((pg.len(), pg[0].id), (2, b2.id), "newest first");
        assert!(store.storage().delete_backup(b2.id).await.unwrap());
        assert_eq!(store.storage().backups(None, None).await.unwrap().len(), 2);
    }
}
