use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::{Profile, ProfileKind};
use sqlx::SqlitePool;
use sqlx::types::Json;

use crate::{NewProfile, ProfileRepo, Result, StateError};

pub(super) struct SqliteProfiles {
    pool: SqlitePool,
}

impl SqliteProfiles {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

struct ProfileRow {
    id: i64,
    kind: String,
    name: String,
    config_json: Json<BTreeMap<String, String>>,
    secret_keys_json: Json<Vec<String>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ProfileRow> for Profile {
    type Error = StateError;

    fn try_from(row: ProfileRow) -> Result<Self> {
        let kind = row
            .kind
            .parse()
            .map_err(|e| StateError::Corrupt(format!("profile {}: {e}", row.id)))?;
        Ok(Profile {
            id: row.id,
            kind,
            name: row.name,
            config: row.config_json.0,
            secret_keys: row.secret_keys_json.0,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[async_trait]
impl ProfileRepo for SqliteProfiles {
    async fn insert(&self, new: NewProfile<'_>) -> Result<Profile> {
        let kind = new.kind.as_str();
        let config = Json(new.config);
        let secret_keys = Json(new.secret_keys);
        let now = Utc::now();
        let row = sqlx::query_as!(
            ProfileRow,
            r#"INSERT INTO profiles (kind, name, config_json, secret_keys_json, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!", kind, name,
                         config_json AS "config_json: Json<BTreeMap<String, String>>",
                         secret_keys_json AS "secret_keys_json: Json<Vec<String>>",
                         created_at AS "created_at: DateTime<Utc>",
                         updated_at AS "updated_at: DateTime<Utc>""#,
            kind,
            new.name,
            config,
            secret_keys,
            now,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StateError::Conflict(format!("profile {kind}/{}", new.name))
            }
            e => e.into(),
        })?;
        row.try_into()
    }

    async fn get(&self, kind: ProfileKind, name: &str) -> Result<Option<Profile>> {
        let kind = kind.as_str();
        sqlx::query_as!(
            ProfileRow,
            r#"SELECT id AS "id!", kind, name,
                      config_json AS "config_json: Json<BTreeMap<String, String>>",
                      secret_keys_json AS "secret_keys_json: Json<Vec<String>>",
                      created_at AS "created_at: DateTime<Utc>",
                      updated_at AS "updated_at: DateTime<Utc>"
               FROM profiles WHERE kind = ? AND name = ?"#,
            kind,
            name,
        )
        .fetch_optional(&self.pool)
        .await?
        .map(Profile::try_from)
        .transpose()
    }

    async fn list(&self, kind: Option<ProfileKind>) -> Result<Vec<Profile>> {
        let kind = kind.map(ProfileKind::as_str);
        sqlx::query_as!(
            ProfileRow,
            r#"SELECT id AS "id!", kind, name,
                      config_json AS "config_json: Json<BTreeMap<String, String>>",
                      secret_keys_json AS "secret_keys_json: Json<Vec<String>>",
                      created_at AS "created_at: DateTime<Utc>",
                      updated_at AS "updated_at: DateTime<Utc>"
               FROM profiles WHERE ?1 IS NULL OR kind = ?1
               ORDER BY kind, name"#,
            kind,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Profile::try_from)
        .collect()
    }

    async fn update(&self, new: NewProfile<'_>) -> Result<Option<Profile>> {
        let kind = new.kind.as_str();
        let config = Json(new.config);
        let secret_keys = Json(new.secret_keys);
        let now = Utc::now();
        sqlx::query_as!(
            ProfileRow,
            r#"UPDATE profiles SET config_json = ?, secret_keys_json = ?, updated_at = ?
               WHERE kind = ? AND name = ?
               RETURNING id AS "id!", kind, name,
                         config_json AS "config_json: Json<BTreeMap<String, String>>",
                         secret_keys_json AS "secret_keys_json: Json<Vec<String>>",
                         created_at AS "created_at: DateTime<Utc>",
                         updated_at AS "updated_at: DateTime<Utc>""#,
            config,
            secret_keys,
            now,
            kind,
            new.name,
        )
        .fetch_optional(&self.pool)
        .await?
        .map(Profile::try_from)
        .transpose()
    }

    async fn delete(&self, kind: ProfileKind, name: &str) -> Result<bool> {
        let kind = kind.as_str();
        let result = sqlx::query!(
            "DELETE FROM profiles WHERE kind = ? AND name = ?",
            kind,
            name
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SqliteStore, StateStore};

    async fn store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = SqliteStore::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        (dir, store)
    }

    fn new_profile<'a>(
        kind: ProfileKind,
        name: &'a str,
        config: &'a BTreeMap<String, String>,
        secret_keys: &'a [String],
    ) -> NewProfile<'a> {
        NewProfile {
            kind,
            name,
            config,
            secret_keys,
        }
    }

    #[tokio::test]
    async fn insert_get_list_delete() {
        let (_dir, store) = store().await;
        let repo = store.profiles();
        let config = BTreeMap::from([("region".to_owned(), "eu-west-1".to_owned())]);
        let secrets = vec!["secret_access_key".to_owned()];

        let created = repo
            .insert(new_profile(ProfileKind::Aws, "acme", &config, &secrets))
            .await
            .unwrap();
        assert_eq!(created.config, config);
        assert_eq!(created.secret_keys, secrets);

        let empty = BTreeMap::new();
        repo.insert(new_profile(ProfileKind::K8s, "kind", &empty, &[]))
            .await
            .unwrap();

        assert_eq!(
            repo.get(ProfileKind::Aws, "acme").await.unwrap(),
            Some(created)
        );
        assert_eq!(repo.get(ProfileKind::Gcp, "acme").await.unwrap(), None);
        assert_eq!(repo.list(None).await.unwrap().len(), 2);
        assert_eq!(
            repo.list(Some(ProfileKind::K8s)).await.unwrap()[0].name,
            "kind"
        );

        assert!(repo.delete(ProfileKind::Aws, "acme").await.unwrap());
        assert!(!repo.delete(ProfileKind::Aws, "acme").await.unwrap());
        assert_eq!(repo.list(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn same_name_is_allowed_across_kinds_but_not_within_one() {
        let (_dir, store) = store().await;
        let repo = store.profiles();
        let empty = BTreeMap::new();

        repo.insert(new_profile(ProfileKind::Aws, "acme", &empty, &[]))
            .await
            .unwrap();
        repo.insert(new_profile(ProfileKind::Gcp, "acme", &empty, &[]))
            .await
            .unwrap();
        let err = repo
            .insert(new_profile(ProfileKind::Aws, "acme", &empty, &[]))
            .await
            .unwrap_err();
        assert!(matches!(err, StateError::Conflict(_)), "{err}");
    }
}
