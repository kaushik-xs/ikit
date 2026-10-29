use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::workload::WorkloadName;
use ikit_core::{PublishedPort, Run, RunResult, ServiceInstance, Workload, WorkloadStatus};
use semver::Version;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};

use crate::{NewRun, NewService, Result, ServiceUpdate, StateError, WorkloadRepo, map_constraint};

pub(super) struct SqliteWorkloads {
    pool: SqlitePool,
}

impl SqliteWorkloads {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

struct WorkloadRow {
    id: i64,
    name: String,
    env: String,
    seq: i64,
    target: String,
    target_kind: String,
    status: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<WorkloadRow> for Workload {
    type Error = StateError;

    fn try_from(r: WorkloadRow) -> Result<Self> {
        let corrupt = |e: String| StateError::Corrupt(format!("workload {}: {e}", r.name));
        Ok(Self {
            id: r.id,
            seq: u32::try_from(r.seq).map_err(|e| corrupt(e.to_string()))?,
            target_kind: r
                .target_kind
                .parse()
                .map_err(|e: ikit_core::UnknownVariant| corrupt(e.to_string()))?,
            status: r
                .status
                .parse()
                .map_err(|e: ikit_core::UnknownVariant| corrupt(e.to_string()))?,
            name: r.name,
            env: r.env,
            target: r.target,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

struct ServiceRow {
    id: i64,
    name: String,
    group_kind: String,
    registry_service: String,
    version_constraint: String,
    version_id: i64,
    version: String,
    image: String,
    digest: Option<String>,
    deprecated: bool,
    env_json: Json<BTreeMap<String, String>>,
    secret_env_json: Json<Vec<String>>,
    publish_json: Json<Vec<PublishedPort>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ServiceRow> for ServiceInstance {
    type Error = StateError;

    fn try_from(r: ServiceRow) -> Result<Self> {
        let corrupt = |e: String| StateError::Corrupt(format!("service {}: {e}", r.name));
        Ok(Self {
            id: r.id,
            group: r
                .group_kind
                .parse()
                .map_err(|e: ikit_core::UnknownVariant| corrupt(e.to_string()))?,
            constraint: r
                .version_constraint
                .parse()
                .map_err(|e: ikit_core::VersionError| corrupt(e.to_string()))?,
            version: Version::parse(&r.version).map_err(|e| corrupt(e.to_string()))?,
            name: r.name,
            registry_service: r.registry_service,
            version_id: r.version_id,
            image: r.image,
            digest: r.digest,
            deprecated: r.deprecated,
            env: r.env_json.0,
            secret_env: r.secret_env_json.0,
            publish: r.publish_json.0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

async fn get(conn: &mut SqliteConnection, name: &str) -> Result<Option<Workload>> {
    sqlx::query_as!(
        WorkloadRow,
        r#"SELECT w.id AS "id!", w.name, w.env, w.seq, t.name AS "target!", t.kind AS "target_kind!", w.status,
                  w.created_at AS "created_at: DateTime<Utc>", w.updated_at AS "updated_at: DateTime<Utc>"
           FROM workloads w JOIN targets t ON t.id = w.target_id
           WHERE w.name = ?"#,
        name,
    )
    .fetch_optional(&mut *conn)
    .await?
    .map(Workload::try_from)
    .transpose()
}

async fn touch(conn: &mut SqliteConnection, workload_id: i64) -> Result<()> {
    let now = Utc::now();
    sqlx::query!(
        "UPDATE workloads SET updated_at = ? WHERE id = ?",
        now,
        workload_id
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[async_trait]
impl WorkloadRepo for SqliteWorkloads {
    async fn create(&self, prefix: &str, env: &str, target_id: i64) -> Result<Workload> {
        let mut tx = self.pool.begin().await?;
        let seq = sqlx::query_scalar!(
            r#"INSERT INTO env_counters (env, next_seq) VALUES (?, 2)
               ON CONFLICT (env) DO UPDATE SET next_seq = next_seq + 1
               RETURNING next_seq - 1 AS "seq!: i64""#,
            env,
        )
        .fetch_one(&mut *tx)
        .await?;
        let seq_u32 = u32::try_from(seq)
            .map_err(|e| StateError::Corrupt(format!("env counter {env}: {e}")))?;
        let name = WorkloadName::new(prefix, env, seq_u32).to_string();
        let now = Utc::now();
        sqlx::query!(
            "INSERT INTO workloads (name, env, seq, target_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
            name,
            env,
            seq,
            target_id,
            now,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(db) if crate::is_foreign_key_violation(db.as_ref()) => {
                StateError::NotFound(format!("target {target_id}"))
            }
            e => map_constraint(format!("workload {name}"))(e),
        })?;
        let workload = get(&mut tx, &name).await?;
        tx.commit().await?;
        workload.ok_or_else(|| StateError::Corrupt(format!("workload {name} vanished")))
    }

    async fn get(&self, name: &str) -> Result<Option<Workload>> {
        get(&mut *self.pool.acquire().await?, name).await
    }

    async fn list(&self, env: Option<&str>) -> Result<Vec<Workload>> {
        sqlx::query_as!(
            WorkloadRow,
            r#"SELECT w.id AS "id!", w.name, w.env, w.seq, t.name AS "target!", t.kind AS "target_kind!", w.status,
                      w.created_at AS "created_at: DateTime<Utc>", w.updated_at AS "updated_at: DateTime<Utc>"
               FROM workloads w JOIN targets t ON t.id = w.target_id
               WHERE ?1 IS NULL OR w.env = ?1
               ORDER BY w.env, w.seq"#,
            env,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Workload::try_from)
        .collect()
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM workloads WHERE name = ?", name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn services(&self, workload_id: i64) -> Result<Vec<ServiceInstance>> {
        sqlx::query_as!(
            ServiceRow,
            r#"SELECT s.id AS "id!", s.name, g.kind AS "group_kind!", r.name AS "registry_service!",
                      s.version_constraint, v.id AS "version_id!", v.version AS "version!", v.image AS "image!",
                      v.digest,
                      v.deprecated AS "deprecated!: bool",
                      s.env_json AS "env_json: Json<BTreeMap<String, String>>",
                      s.secret_env_json AS "secret_env_json: Json<Vec<String>>",
                      s.publish_json AS "publish_json: Json<Vec<PublishedPort>>",
                      s.created_at AS "created_at: DateTime<Utc>", s.updated_at AS "updated_at: DateTime<Utc>"
               FROM services s
               JOIN service_groups g ON g.id = s.group_id
               JOIN registry_services r ON r.id = s.registry_service_id
               JOIN registry_versions v ON v.id = s.registry_version_id
               WHERE s.workload_id = ?
               ORDER BY CASE g.kind WHEN 'app' THEN 0 WHEN 'platform' THEN 1 ELSE 2 END, s.name"#,
            workload_id,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(ServiceInstance::try_from)
        .collect()
    }

    async fn add_services(&self, workload_id: i64, new: &[NewService<'_>]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();
        for s in new {
            let kind = s.group.as_str();
            let group_id = sqlx::query_scalar!(
                r#"INSERT INTO service_groups (workload_id, kind, created_at) VALUES (?, ?, ?)
                   ON CONFLICT (workload_id, kind) DO UPDATE SET kind = excluded.kind
                   RETURNING id AS "id!""#,
                workload_id,
                kind,
                now,
            )
            .fetch_one(&mut *tx)
            .await?;
            let constraint = s.constraint.to_string();
            let env = Json(s.env);
            let secret_env = Json(s.secret_env);
            sqlx::query!(
                "INSERT INTO services
                     (workload_id, group_id, name, registry_service_id, registry_version_id, version_constraint,
                      env_json, secret_env_json, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                workload_id,
                group_id,
                s.name,
                s.registry_service_id,
                s.version_id,
                constraint,
                env,
                secret_env,
                now,
                now,
            )
            .execute(&mut *tx)
            .await
            .map_err(map_constraint(format!("service {}", s.name)))?;
        }
        touch(&mut tx, workload_id).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn update_service(
        &self,
        workload_id: i64,
        name: &str,
        update: ServiceUpdate<'_>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let constraint = update.constraint.to_string();
        let env = Json(update.env);
        let secret_env = Json(update.secret_env);
        let publish = Json(update.publish);
        let now = Utc::now();
        let result = sqlx::query!(
            "UPDATE services
             SET registry_version_id = ?, version_constraint = ?, env_json = ?, secret_env_json = ?,
                 publish_json = ?, updated_at = ?
             WHERE workload_id = ? AND name = ?",
            update.version_id,
            constraint,
            env,
            secret_env,
            publish,
            now,
            workload_id,
            name,
        )
        .execute(&mut *tx)
        .await?;
        touch(&mut tx, workload_id).await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    async fn remove_services(&self, workload_id: i64, names: &[String]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let mut removed = 0;
        for name in names {
            removed += sqlx::query!(
                "DELETE FROM services WHERE workload_id = ? AND name = ?",
                workload_id,
                name
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        sqlx::query!(
            "DELETE FROM service_groups
             WHERE workload_id = ? AND NOT EXISTS (SELECT 1 FROM services WHERE group_id = service_groups.id)",
            workload_id,
        )
        .execute(&mut *tx)
        .await?;
        touch(&mut tx, workload_id).await?;
        tx.commit().await?;
        Ok(removed)
    }

    async fn set_status(&self, workload_id: i64, status: WorkloadStatus) -> Result<()> {
        let status = status.as_str();
        let now = Utc::now();
        sqlx::query!(
            "UPDATE workloads SET status = ?, updated_at = ? WHERE id = ?",
            status,
            now,
            workload_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn try_lock(
        &self,
        workload_id: i64,
        holder: &str,
        stale_before: DateTime<Utc>,
    ) -> Result<Option<String>> {
        let now = Utc::now();
        let taken = sqlx::query_scalar!(
            r#"INSERT INTO locks (workload_id, holder, acquired_at) VALUES (?, ?, ?)
               ON CONFLICT (workload_id) DO UPDATE SET holder = excluded.holder, acquired_at = excluded.acquired_at
               WHERE locks.acquired_at < ? OR locks.holder = excluded.holder
               RETURNING holder AS "holder!""#,
            workload_id,
            holder,
            now,
            stale_before,
        )
        .fetch_optional(&self.pool)
        .await?;
        if taken.is_some() {
            return Ok(None);
        }
        let current = sqlx::query_scalar!(
            "SELECT holder FROM locks WHERE workload_id = ?",
            workload_id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(Some(current.unwrap_or_default()))
    }

    async fn unlock(&self, workload_id: i64, holder: &str) -> Result<()> {
        sqlx::query!(
            "DELETE FROM locks WHERE workload_id = ? AND holder = ?",
            workload_id,
            holder
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn start_run(&self, workload_id: i64, run: NewRun<'_>) -> Result<i64> {
        let action = run.action.as_str();
        let now = Utc::now();
        Ok(sqlx::query_scalar!(
            r#"INSERT INTO runs (workload_id, action, actor, plan_json, compose_yaml, started_at)
               VALUES (?, ?, ?, ?, ?, ?) RETURNING id AS "id!""#,
            workload_id,
            action,
            run.actor,
            run.plan_json,
            run.compose_yaml,
            now,
        )
        .fetch_one(&self.pool)
        .await?)
    }

    async fn finish_run(&self, run_id: i64, result: RunResult, error: Option<&str>) -> Result<()> {
        let result = result.as_str();
        let now = Utc::now();
        sqlx::query!(
            "UPDATE runs SET result = ?, error = ?, finished_at = ? WHERE id = ?",
            result,
            error,
            now,
            run_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn runs(&self, workload_id: i64, limit: i64) -> Result<Vec<Run>> {
        let rows = sqlx::query!(
            r#"SELECT id AS "id!", action, actor, result, error, plan_json,
                      started_at AS "started_at: DateTime<Utc>", finished_at AS "finished_at: DateTime<Utc>"
               FROM runs WHERE workload_id = ? ORDER BY id DESC LIMIT ?"#,
            workload_id,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let corrupt = |e: ikit_core::UnknownVariant| {
                    StateError::Corrupt(format!("run {}: {e}", r.id))
                };
                Ok(Run {
                    id: r.id,
                    action: r.action.parse().map_err(corrupt)?,
                    result: r.result.parse().map_err(corrupt)?,
                    actor: r.actor,
                    error: r.error,
                    plan: r.plan_json,
                    started_at: r.started_at,
                    finished_at: r.finished_at,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ikit_core::registry::{ServiceDef, Source, VersionDef};
    use ikit_core::target::TargetOrigin;
    use ikit_core::{Category, Connection, ServiceSpec, VersionConstraint};

    use super::*;
    use crate::{SqliteStore, StateStore};

    async fn store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = SqliteStore::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn numbers_workloads_per_env() {
        let (_dir, store) = store().await;
        let target = store
            .targets()
            .insert("local", TargetOrigin::Attached, &Connection::Local)
            .await
            .unwrap();
        let repo = store.workloads();

        let names: Vec<String> = [
            repo.create("wl", "dev", target.id).await.unwrap(),
            repo.create("wl", "dev", target.id).await.unwrap(),
            repo.create("wl", "prod", target.id).await.unwrap(),
        ]
        .into_iter()
        .map(|w| w.name)
        .collect();
        assert_eq!(names, ["wl-dev-001", "wl-dev-002", "wl-prod-001"]);

        // Numbers are never reused, even after a delete.
        assert!(repo.delete("wl-dev-002").await.unwrap());
        assert_eq!(
            repo.create("wl", "dev", target.id).await.unwrap().name,
            "wl-dev-003"
        );
        assert_eq!(repo.list(Some("dev")).await.unwrap().len(), 2);
        assert!(matches!(
            repo.create("wl", "dev", 9999).await,
            Err(StateError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn services_groups_and_registry_protection() {
        let (_dir, store) = store().await;
        let target = store
            .targets()
            .insert("local", TargetOrigin::Attached, &Connection::Local)
            .await
            .unwrap();
        let version = Version::new(16, 4, 0);
        let entry = store
            .registry()
            .add_service(
                &ServiceDef {
                    name: "postgres".into(),
                    display_name: None,
                    description: None,
                    category: Category::Data,
                    tags: vec![],
                    owner: None,
                    homepage: None,
                    versions: vec![VersionDef::new(
                        version.clone(),
                        "postgres:16.4".into(),
                        ServiceSpec::default(),
                    )],
                },
                Source::Manual,
            )
            .await
            .unwrap();
        let wl = store
            .workloads()
            .create("wl", "dev", target.id)
            .await
            .unwrap();

        let constraint: VersionConstraint = "^16".parse().unwrap();
        let env = BTreeMap::from([("POSTGRES_DB".to_owned(), "app".to_owned())]);
        let secret_env = vec!["POSTGRES_PASSWORD".to_owned()];
        let new = NewService {
            name: "postgres",
            group: Category::Data,
            registry_service_id: entry.service.id,
            version_id: entry.versions[0].id,
            constraint: &constraint,
            env: &env,
            secret_env: &secret_env,
        };
        store.workloads().add_services(wl.id, &[new]).await.unwrap();
        assert!(matches!(
            store.workloads().add_services(wl.id, &[new]).await,
            Err(StateError::Conflict(_))
        ));

        let services = store.workloads().services(wl.id).await.unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].registry_service, "postgres");
        assert_eq!(services[0].version, version);
        assert_eq!(services[0].env, env);

        // Registry entries and the target are protected while used.
        assert!(matches!(
            store.registry().delete_service("postgres").await,
            Err(StateError::InUse(_))
        ));
        assert!(matches!(
            store.registry().delete_version("postgres", &version).await,
            Err(StateError::InUse(_))
        ));
        assert!(matches!(
            store.targets().delete("local").await,
            Err(StateError::InUse(_))
        ));
        assert_eq!(
            store.registry().usage("postgres").await.unwrap()[0].workload,
            "wl-dev-001"
        );

        assert_eq!(
            store
                .workloads()
                .remove_services(wl.id, &["postgres".to_owned()])
                .await
                .unwrap(),
            1
        );
        assert!(store.registry().delete_service("postgres").await.unwrap());
    }

    #[tokio::test]
    async fn locks_and_runs() {
        let (_dir, store) = store().await;
        let target = store
            .targets()
            .insert("local", TargetOrigin::Attached, &Connection::Local)
            .await
            .unwrap();
        let wl = store
            .workloads()
            .create("wl", "dev", target.id)
            .await
            .unwrap();
        let repo = store.workloads();
        let long_ago = Utc::now() - chrono::Duration::hours(1);

        assert_eq!(repo.try_lock(wl.id, "a", long_ago).await.unwrap(), None);
        assert_eq!(
            repo.try_lock(wl.id, "a", long_ago).await.unwrap(),
            None,
            "re-entrant for the holder"
        );
        assert_eq!(
            repo.try_lock(wl.id, "b", long_ago)
                .await
                .unwrap()
                .as_deref(),
            Some("a")
        );
        // A lock older than the cutoff can be taken over.
        assert_eq!(
            repo.try_lock(wl.id, "b", Utc::now() + chrono::Duration::seconds(1))
                .await
                .unwrap(),
            None
        );
        repo.unlock(wl.id, "b").await.unwrap();
        assert_eq!(repo.try_lock(wl.id, "c", long_ago).await.unwrap(), None);

        let run = NewRun {
            action: ikit_core::RunAction::Apply,
            actor: "test",
            plan_json: "{}",
            compose_yaml: Some("x"),
        };
        let id = repo.start_run(wl.id, run).await.unwrap();
        repo.finish_run(id, RunResult::Failed, Some("boom"))
            .await
            .unwrap();
        let runs = repo.runs(wl.id, 10).await.unwrap();
        assert_eq!(
            (runs[0].result, runs[0].error.as_deref()),
            (RunResult::Failed, Some("boom"))
        );

        repo.set_status(wl.id, WorkloadStatus::Applied)
            .await
            .unwrap();
        assert_eq!(
            repo.get(&wl.name).await.unwrap().unwrap().status,
            WorkloadStatus::Applied
        );
    }
}
