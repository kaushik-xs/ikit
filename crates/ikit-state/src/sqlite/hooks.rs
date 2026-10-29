use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::{Hook, HookAction, HookKind, HookRunRecord, SqlAction, SqlVia, UnknownVariant};
use sqlx::SqlitePool;

use crate::{HookRepo, NewHook, Result, ScriptRun, StateError, map_constraint};

pub(super) struct SqliteHooks {
    pool: SqlitePool,
}

impl SqliteHooks {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

struct HookRow {
    id: i64,
    name: String,
    kind: String,
    phase: String,
    service: String,
    engine: String,
    database: Option<String>,
    source: String,
    spec_json: String,
    fail_on_error: bool,
    ord: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<HookRow> for Hook {
    type Error = StateError;

    fn try_from(r: HookRow) -> Result<Self> {
        let corrupt = |e: String| StateError::Corrupt(format!("hook {}: {e}", r.name));
        let action = if r.spec_json.trim() != "{}" {
            serde_json::from_str::<HookAction>(&r.spec_json).map_err(|e| corrupt(e.to_string()))?
        } else {
            // A 0006-era SQL hook.
            let _: HookKind = r
                .kind
                .parse()
                .map_err(|e: UnknownVariant| corrupt(e.to_string()))?;
            HookAction::Sql(SqlAction {
                service: r.service.clone(),
                engine: r
                    .engine
                    .parse()
                    .map_err(|e: UnknownVariant| corrupt(e.to_string()))?,
                database: r.database.clone(),
                source: PathBuf::from(&r.source),
                via: SqlVia::Exec,
            })
        };
        Ok(Self {
            id: r.id,
            phase: r
                .phase
                .parse()
                .map_err(|e: UnknownVariant| corrupt(e.to_string()))?,
            fail_on_error: r.fail_on_error,
            action,
            name: r.name,
            ord: r.ord,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

#[async_trait]
impl HookRepo for SqliteHooks {
    async fn insert(&self, workload_id: i64, new: NewHook<'_>) -> Result<Hook> {
        let kind = new.action.kind().as_str();
        let phase = new.phase.as_str();
        let service = new.action.service().unwrap_or_default();
        let (engine, database, source) = match new.action {
            HookAction::Sql(a) => (
                a.engine.as_str(),
                a.database.as_deref(),
                a.source.to_string_lossy().into_owned(),
            ),
            _ => ("", None, String::new()),
        };
        let spec = serde_json::to_string(new.action).expect("hook action serializes");
        let now = Utc::now();
        sqlx::query_as!(
            HookRow,
            r#"INSERT INTO hooks (workload_id, name, kind, phase, service, engine, database, source, spec_json,
                                  fail_on_error, ord, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                       (SELECT COALESCE(MAX(ord), 0) + 1 FROM hooks WHERE workload_id = ?), ?, ?)
               RETURNING id AS "id!", name, kind, phase, service, engine, database, source, spec_json,
                         fail_on_error AS "fail_on_error: bool", ord AS "ord!",
                         created_at AS "created_at: DateTime<Utc>", updated_at AS "updated_at: DateTime<Utc>""#,
            workload_id,
            new.name,
            kind,
            phase,
            service,
            engine,
            database,
            source,
            spec,
            new.fail_on_error,
            workload_id,
            now,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_constraint(format!("hook {}", new.name)))?
        .try_into()
    }

    async fn list(&self, workload_id: i64) -> Result<Vec<Hook>> {
        sqlx::query_as!(
            HookRow,
            r#"SELECT id AS "id!", name, kind, phase, service, engine, database, source, spec_json,
                      fail_on_error AS "fail_on_error: bool", ord,
                      created_at AS "created_at: DateTime<Utc>", updated_at AS "updated_at: DateTime<Utc>"
               FROM hooks WHERE workload_id = ? ORDER BY ord"#,
            workload_id,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Hook::try_from)
        .collect()
    }

    async fn delete(&self, workload_id: i64, name: &str) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM hooks WHERE workload_id = ? AND name = ?",
            workload_id,
            name
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn record(&self, hook_id: i64, run: ScriptRun<'_>) -> Result<()> {
        let result = run.result.as_str();
        let now = Utc::now();
        sqlx::query!(
            "INSERT INTO hook_runs (hook_id, run_id, script, checksum, result, error, ran_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            hook_id,
            run.run_id,
            run.script,
            run.checksum,
            result,
            run.error,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn history(
        &self,
        workload_id: i64,
        hook: Option<&str>,
        limit: i64,
    ) -> Result<Vec<HookRunRecord>> {
        let rows = sqlx::query!(
            r#"SELECT h.name AS "hook!", r.script, r.checksum, r.result, r.error,
                      r.ran_at AS "ran_at: DateTime<Utc>"
               FROM hook_runs r JOIN hooks h ON h.id = r.hook_id
               WHERE h.workload_id = ?1 AND (?2 IS NULL OR h.name = ?2)
               ORDER BY r.id DESC LIMIT ?3"#,
            workload_id,
            hook,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(HookRunRecord {
                    result: r
                        .result
                        .parse()
                        .map_err(|e: UnknownVariant| StateError::Corrupt(e.to_string()))?,
                    hook: r.hook,
                    script: r.script,
                    checksum: r.checksum,
                    error: r.error,
                    ran_at: r.ran_at,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ikit_core::target::TargetOrigin;
    use ikit_core::{Connection, HookPhase, RunResult, SqlEngine};

    use super::*;
    use crate::{SqliteStore, StateStore};

    #[tokio::test]
    async fn hooks_keep_order_and_history() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = SqliteStore::open(&dir.path().join("state.db"))
            .await
            .unwrap();
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
        let action = HookAction::Sql(SqlAction {
            service: "postgres".into(),
            engine: SqlEngine::Postgres,
            database: None,
            source: "/tmp/sql".into(),
            via: SqlVia::Exec,
        });
        let new = |name| NewHook {
            name,
            phase: HookPhase::PostApply,
            fail_on_error: true,
            action: &action,
        };
        let schema = store.hooks().insert(wl.id, new("schema")).await.unwrap();
        let seed = store.hooks().insert(wl.id, new("seed")).await.unwrap();
        assert_eq!((schema.ord, seed.ord), (1, 2));
        assert!(matches!(
            store.hooks().insert(wl.id, new("seed")).await,
            Err(StateError::Conflict(_))
        ));

        let run = ScriptRun {
            run_id: None,
            script: "001.sql",
            checksum: "abc",
            result: RunResult::Ok,
            error: None,
        };
        store.hooks().record(seed.id, run).await.unwrap();
        assert_eq!(
            store
                .hooks()
                .history(wl.id, Some("seed"), 10)
                .await
                .unwrap()[0]
                .script,
            "001.sql"
        );
        assert!(
            store
                .hooks()
                .history(wl.id, Some("schema"), 10)
                .await
                .unwrap()
                .is_empty()
        );

        let shell = HookAction::Shell(ikit_core::ShellAction {
            command: "echo hi".into(),
            service: None,
        });
        let notify = store
            .hooks()
            .insert(
                wl.id,
                NewHook {
                    name: "notify",
                    phase: HookPhase::PreDestroy,
                    fail_on_error: false,
                    action: &shell,
                },
            )
            .await
            .unwrap();
        assert_eq!(notify.action, shell);
        assert!(!notify.fail_on_error);
        assert_eq!(store.hooks().list(wl.id).await.unwrap()[1].action, action);

        assert!(store.hooks().delete(wl.id, "schema").await.unwrap());
        assert_eq!(store.hooks().list(wl.id).await.unwrap().len(), 2);
    }
}
