use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::{
    Backend, BackendOptions, Binding, Identity, Resource, ResourceApi, UnknownVariant,
};
use sqlx::SqlitePool;
use sqlx::types::Json;

use crate::{
    IdentityUpdate, NewBackend, NewIdentity, NewResource, ResourceRepo, ResourceUpdate, Result,
    StateError, map_constraint,
};

pub(super) struct SqliteResources {
    pool: SqlitePool,
}

impl SqliteResources {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn backend_id(&self, name: &str) -> Result<i64> {
        sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM resource_backends WHERE name = ?"#,
            name
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StateError::NotFound(format!("backend {name}")))
    }
}

fn corrupt(e: UnknownVariant) -> StateError {
    StateError::Corrupt(e.to_string())
}

struct BackendRow {
    id: i64,
    name: String,
    api: String,
    provider: String,
    profile: Option<String>,
    options_json: Json<BackendOptions>,
    created_at: DateTime<Utc>,
}

impl TryFrom<BackendRow> for Backend {
    type Error = StateError;

    fn try_from(r: BackendRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            api: r.api.parse().map_err(corrupt)?,
            provider: r.provider.parse().map_err(corrupt)?,
            name: r.name,
            profile: r.profile,
            options: r.options_json.0,
            created_at: r.created_at,
        })
    }
}

struct ResourceRow {
    id: i64,
    workload: String,
    service: String,
    name: String,
    api: String,
    backend: Option<String>,
    external_id: String,
    outputs_json: Json<BTreeMap<String, String>>,
    origin: String,
    status: String,
    orphaned: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ResourceRow> for Resource {
    type Error = StateError;

    fn try_from(r: ResourceRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            api: r.api.parse().map_err(corrupt)?,
            origin: r.origin.parse().map_err(corrupt)?,
            status: r.status.parse().map_err(corrupt)?,
            workload: r.workload,
            service: r.service,
            name: r.name,
            backend: r.backend,
            external_id: r.external_id,
            outputs: r.outputs_json.0,
            orphaned: r.orphaned != 0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

struct IdentityRow {
    id: i64,
    workload: String,
    service: String,
    profile: String,
    account: Option<String>,
    user_name: String,
    user_arn: Option<String>,
    access_key_id: Option<String>,
    retiring_key_id: Option<String>,
    covers_json: Json<Vec<String>>,
    status: String,
    orphaned: i64,
    created_at: DateTime<Utc>,
    rotated_at: Option<DateTime<Utc>>,
}

impl TryFrom<IdentityRow> for Identity {
    type Error = StateError;

    fn try_from(r: IdentityRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            status: r.status.parse().map_err(corrupt)?,
            workload: r.workload,
            service: r.service,
            profile: r.profile,
            account: r.account,
            user_name: r.user_name,
            user_arn: r.user_arn,
            access_key_id: r.access_key_id,
            retiring_key_id: r.retiring_key_id,
            covers: r.covers_json.0,
            orphaned: r.orphaned != 0,
            created_at: r.created_at,
            rotated_at: r.rotated_at,
        })
    }
}

#[async_trait]
impl ResourceRepo for SqliteResources {
    async fn insert_backend(&self, new: NewBackend<'_>) -> Result<Backend> {
        let api = new.api.as_str();
        let provider = new.provider.as_str();
        let options = Json(new.options);
        let now = Utc::now();
        sqlx::query_as!(
            BackendRow,
            r#"INSERT INTO resource_backends (name, api, provider, profile, options_json, created_at)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!", name, api, provider, profile,
                         options_json AS "options_json: Json<BackendOptions>",
                         created_at AS "created_at: DateTime<Utc>""#,
            new.name,
            api,
            provider,
            new.profile,
            options,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_constraint(format!("backend {}", new.name)))?
        .try_into()
    }

    async fn backend(&self, name: &str) -> Result<Option<Backend>> {
        sqlx::query_as!(
            BackendRow,
            r#"SELECT id AS "id!", name, api, provider, profile,
                      options_json AS "options_json: Json<BackendOptions>",
                      created_at AS "created_at: DateTime<Utc>"
               FROM resource_backends WHERE name = ?"#,
            name,
        )
        .fetch_optional(&self.pool)
        .await?
        .map(Backend::try_from)
        .transpose()
    }

    async fn backends(&self) -> Result<Vec<Backend>> {
        sqlx::query_as!(
            BackendRow,
            r#"SELECT id AS "id!", name, api, provider, profile,
                      options_json AS "options_json: Json<BackendOptions>",
                      created_at AS "created_at: DateTime<Utc>"
               FROM resource_backends ORDER BY name"#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Backend::try_from)
        .collect()
    }

    async fn delete_backend(&self, name: &str) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM resource_backends WHERE name = ?", name)
            .execute(&self.pool)
            .await
            .map_err(map_constraint(format!("backend {name}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn bindings(&self) -> Result<Vec<Binding>> {
        let rows = sqlx::query!(
            r#"SELECT b.env, b.api, r.name AS backend
               FROM resource_bindings b JOIN resource_backends r ON r.id = b.backend_id
               ORDER BY b.env, b.api"#
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(Binding {
                    env: r.env,
                    api: r.api.parse().map_err(corrupt)?,
                    backend: r.backend,
                })
            })
            .collect()
    }

    async fn bind(&self, env: &str, backend: &str) -> Result<Binding> {
        let b = self
            .backend(backend)
            .await?
            .ok_or_else(|| StateError::NotFound(format!("backend {backend}")))?;
        let api = b.api.as_str();
        sqlx::query!(
            r#"INSERT INTO resource_bindings (env, api, backend_id) VALUES (?, ?, ?)
               ON CONFLICT (env, api) DO UPDATE SET backend_id = excluded.backend_id"#,
            env,
            api,
            b.id,
        )
        .execute(&self.pool)
        .await?;
        Ok(Binding {
            env: env.to_owned(),
            api: b.api,
            backend: b.name,
        })
    }

    async fn unbind(&self, env: &str, api: ResourceApi) -> Result<bool> {
        let api = api.as_str();
        let result = sqlx::query!(
            "DELETE FROM resource_bindings WHERE env = ? AND api = ?",
            env,
            api
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn list(&self, workload_id: Option<i64>) -> Result<Vec<Resource>> {
        sqlx::query_as!(
            ResourceRow,
            r#"SELECT r.id AS "id!", w.name AS workload, r.service, r.name, r.api,
                      b.name AS "backend?", r.external_id,
                      r.outputs_json AS "outputs_json: Json<BTreeMap<String, String>>",
                      r.origin, r.status, r.orphaned,
                      r.created_at AS "created_at: DateTime<Utc>",
                      r.updated_at AS "updated_at: DateTime<Utc>"
               FROM workload_resources r
               JOIN workloads w ON w.id = r.workload_id
               LEFT JOIN resource_backends b ON b.id = r.backend_id
               WHERE ?1 IS NULL OR r.workload_id = ?1
               ORDER BY w.name, r.service, r.name"#,
            workload_id,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Resource::try_from)
        .collect()
    }

    async fn insert(&self, workload_id: i64, new: NewResource<'_>) -> Result<Resource> {
        let backend_id = match new.backend {
            Some(b) => Some(self.backend_id(b).await?),
            None => None,
        };
        let api = new.api.as_str();
        let origin = new.origin.as_str();
        let status = new.status.as_str();
        let outputs = Json(new.outputs);
        let now = Utc::now();
        let id = sqlx::query_scalar!(
            r#"INSERT INTO workload_resources
                   (workload_id, service, name, api, backend_id, external_id, outputs_json,
                    origin, status, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!""#,
            workload_id,
            new.service,
            new.name,
            api,
            backend_id,
            new.external_id,
            outputs,
            origin,
            status,
            now,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_constraint(format!(
            "resource {}.{}",
            new.service, new.name
        )))?;
        self.list(Some(workload_id))
            .await?
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| StateError::NotFound(format!("resource {id}")))
    }

    async fn update(&self, id: i64, u: ResourceUpdate<'_>) -> Result<()> {
        let backend_id = match u.backend {
            Some(b) => Some(self.backend_id(b).await?),
            None => None,
        };
        let origin = u.origin.as_str();
        let status = u.status.as_str();
        let outputs = Json(u.outputs);
        let orphaned = i64::from(u.orphaned);
        let now = Utc::now();
        let result = sqlx::query!(
            r#"UPDATE workload_resources
               SET backend_id = ?, external_id = ?, outputs_json = ?, origin = ?, status = ?,
                   orphaned = ?, updated_at = ?
               WHERE id = ?"#,
            backend_id,
            u.external_id,
            outputs,
            origin,
            status,
            orphaned,
            now,
            id,
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StateError::NotFound(format!("resource {id}")));
        }
        Ok(())
    }

    async fn delete(&self, id: i64) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM workload_resources WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_backend_options(&self, name: &str, options: &BackendOptions) -> Result<()> {
        let options = Json(options);
        let result = sqlx::query!(
            "UPDATE resource_backends SET options_json = ? WHERE name = ?",
            options,
            name
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StateError::NotFound(format!("backend {name}")));
        }
        Ok(())
    }

    async fn identities(&self, workload_id: Option<i64>) -> Result<Vec<Identity>> {
        sqlx::query_as!(
            IdentityRow,
            r#"SELECT i.id AS "id!", w.name AS workload, i.service, i.profile, i.account,
                      i.user_name, i.user_arn, i.access_key_id, i.retiring_key_id,
                      i.covers_json AS "covers_json: Json<Vec<String>>",
                      i.status, i.orphaned,
                      i.created_at AS "created_at: DateTime<Utc>",
                      i.rotated_at AS "rotated_at: DateTime<Utc>"
               FROM service_identities i
               JOIN workloads w ON w.id = i.workload_id
               WHERE ?1 IS NULL OR i.workload_id = ?1
               ORDER BY w.name, i.service, i.profile"#,
            workload_id,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Identity::try_from)
        .collect()
    }

    async fn insert_identity(&self, workload_id: i64, new: NewIdentity<'_>) -> Result<Identity> {
        let now = Utc::now();
        let id = sqlx::query_scalar!(
            r#"INSERT INTO service_identities
                   (workload_id, service, profile, user_name, status, created_at)
               VALUES (?, ?, ?, ?, 'pending', ?)
               RETURNING id AS "id!""#,
            workload_id,
            new.service,
            new.profile,
            new.user_name,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_constraint(format!("identity of {}", new.service)))?;
        self.identities(Some(workload_id))
            .await?
            .into_iter()
            .find(|i| i.id == id)
            .ok_or_else(|| StateError::NotFound(format!("identity {id}")))
    }

    async fn update_identity(&self, id: i64, u: IdentityUpdate<'_>) -> Result<()> {
        let covers = Json(u.covers);
        let status = u.status.as_str();
        let orphaned = i64::from(u.orphaned);
        let result = sqlx::query!(
            r#"UPDATE service_identities
               SET account = ?, user_arn = ?, access_key_id = ?, retiring_key_id = ?,
                   covers_json = ?, status = ?, orphaned = ?, rotated_at = ?
               WHERE id = ?"#,
            u.account,
            u.user_arn,
            u.access_key_id,
            u.retiring_key_id,
            covers,
            status,
            orphaned,
            u.rotated_at,
            id,
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StateError::NotFound(format!("identity {id}")));
        }
        Ok(())
    }

    async fn delete_identity(&self, id: i64) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM service_identities WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use ikit_core::{ResourceOrigin, ResourceProvider, ResourceStatus};

    use crate::{
        IdentityUpdate, NewBackend, NewIdentity, NewResource, ResourceUpdate, SqliteStore,
        StateError, StateStore,
    };

    use super::*;

    /// A store with target `t` and workload `wl-acme-dev-001` on it.
    async fn store() -> (tempfile::TempDir, SqliteStore, i64) {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = SqliteStore::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        let target = store
            .targets()
            .insert(
                "t",
                ikit_core::target::TargetOrigin::Attached,
                &ikit_core::Connection::Local,
            )
            .await
            .unwrap();
        let wl = store
            .workloads()
            .create("wl-acme", "dev", target.id)
            .await
            .unwrap();
        (dir, store, wl.id)
    }

    fn backend<'a>(name: &'a str, api: ResourceApi, options: &'a BackendOptions) -> NewBackend<'a> {
        NewBackend {
            name,
            api,
            provider: ResourceProvider::Aws,
            profile: None,
            options,
        }
    }

    #[tokio::test]
    async fn backends_bindings_and_resources() {
        let (_dir, store, wl) = store().await;
        let r = store.resources();
        let opts = BackendOptions {
            region: Some("ap-south-1".into()),
            ..Default::default()
        };
        r.insert_backend(backend("aws-sqs", ResourceApi::Sqs, &opts))
            .await
            .unwrap();
        r.insert_backend(backend("aws-s3", ResourceApi::S3, &opts))
            .await
            .unwrap();
        assert!(matches!(
            r.insert_backend(backend("aws-s3", ResourceApi::S3, &opts))
                .await,
            Err(StateError::Conflict(_))
        ));
        assert_eq!(r.backend("aws-sqs").await.unwrap().unwrap().options, opts);

        let b = r.bind("dev", "aws-sqs").await.unwrap();
        assert_eq!(b.api, ResourceApi::Sqs);
        r.bind("dev", "aws-s3").await.unwrap();
        assert_eq!(r.bindings().await.unwrap().len(), 2);
        assert!(matches!(
            r.bind("dev", "nope").await,
            Err(StateError::NotFound(_))
        ));

        let outputs = BTreeMap::new();
        let jobs = r
            .insert(
                wl,
                NewResource {
                    service: "sim",
                    name: "jobs",
                    api: ResourceApi::Sqs,
                    backend: None,
                    external_id: "wl-acme-dev-001-sim-jobs",
                    outputs: &outputs,
                    origin: ResourceOrigin::Created,
                    status: ResourceStatus::Pending,
                },
            )
            .await
            .unwrap();
        assert_eq!(jobs.workload, "wl-acme-dev-001");
        assert_eq!(jobs.backend, None);

        // Ready on a backend: the backend can no longer be removed or unbound away.
        let mut ready = jobs.clone();
        ready.backend = Some("aws-sqs".into());
        ready.status = ResourceStatus::Ready;
        ready.outputs.insert("url".into(), "https://sqs/1/q".into());
        r.update(ready.id, ResourceUpdate::from(&ready))
            .await
            .unwrap();
        let listed = r.list(Some(wl)).await.unwrap();
        assert_eq!(listed[0].backend.as_deref(), Some("aws-sqs"));
        assert_eq!(listed[0].outputs["url"], "https://sqs/1/q");
        assert!(r.unbind("dev", ResourceApi::Sqs).await.unwrap());
        assert!(matches!(
            r.delete_backend("aws-sqs").await,
            Err(StateError::InUse(_))
        ));
        // Bound only: in use as well.
        assert!(matches!(
            r.delete_backend("aws-s3").await,
            Err(StateError::InUse(_))
        ));
        assert!(r.unbind("dev", ResourceApi::S3).await.unwrap());
        assert!(r.delete_backend("aws-s3").await.unwrap());

        // Identities.
        let ident = r
            .insert_identity(
                wl,
                NewIdentity {
                    service: "sim",
                    profile: "acme",
                    user_name: "wl-acme-dev-001-sim",
                },
            )
            .await
            .unwrap();
        assert_eq!(ident.status, ResourceStatus::Pending);
        let mut ready = ident.clone();
        ready.status = ResourceStatus::Ready;
        ready.access_key_id = Some("AKIA1".into());
        ready.covers = vec!["sim.jobs".into()];
        r.update_identity(ready.id, IdentityUpdate::from(&ready))
            .await
            .unwrap();
        assert_eq!(
            r.identities(Some(wl)).await.unwrap(),
            [Identity { ..ready }]
        );
        let mut opts2 = opts.clone();
        opts2.per_service_keys = true;
        r.set_backend_options("aws-sqs", &opts2).await.unwrap();
        assert!(
            r.backend("aws-sqs")
                .await
                .unwrap()
                .unwrap()
                .options
                .per_service_keys
        );

        // Workload removal cascades.
        store.workloads().delete("wl-acme-dev-001").await.unwrap();
        assert!(r.list(None).await.unwrap().is_empty());
        assert!(r.identities(None).await.unwrap().is_empty());
        assert!(r.delete_backend("aws-sqs").await.unwrap());
    }
}
