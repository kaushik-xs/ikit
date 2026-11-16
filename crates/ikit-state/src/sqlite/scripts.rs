use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::{Script, ScriptRunRecord, ScriptSpec, UnknownVariant};
use sqlx::SqlitePool;

use crate::{NewScript, Result, ScriptExecution, ScriptRepo, StateError};

pub(super) struct SqliteScripts {
    pool: SqlitePool,
}

impl SqliteScripts {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

struct ScriptRow {
    id: i64,
    name: String,
    description: String,
    interpreter: String,
    runs_on: String,
    body: String,
    checksum: String,
    spec_json: String,
    source: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ScriptRow> for Script {
    type Error = StateError;

    fn try_from(r: ScriptRow) -> Result<Self> {
        let corrupt = |e: String| StateError::Corrupt(format!("script {}: {e}", r.name));
        Ok(Self {
            id: r.id,
            interpreter: r
                .interpreter
                .parse()
                .map_err(|e: UnknownVariant| corrupt(e.to_string()))?,
            runs_on: r.runs_on.parse().map_err(corrupt)?,
            spec: serde_json::from_str::<ScriptSpec>(&r.spec_json)
                .map_err(|e| corrupt(e.to_string()))?,
            name: r.name,
            description: r.description,
            body: r.body,
            checksum: r.checksum,
            source: r.source,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

#[async_trait]
impl ScriptRepo for SqliteScripts {
    async fn upsert(&self, new: NewScript<'_>) -> Result<Script> {
        let interpreter = new.interpreter.as_str();
        let runs_on = new.runs_on.to_string();
        let spec = serde_json::to_string(new.spec).expect("script spec serializes");
        let now = Utc::now();
        sqlx::query_as!(
            ScriptRow,
            r#"INSERT INTO scripts (name, description, interpreter, runs_on, body, checksum, spec_json,
                                    source, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
               ON CONFLICT (name) DO UPDATE SET
                   description = excluded.description, interpreter = excluded.interpreter,
                   runs_on = excluded.runs_on, body = excluded.body, checksum = excluded.checksum,
                   spec_json = excluded.spec_json, source = excluded.source,
                   updated_at = excluded.updated_at
               RETURNING id AS "id!", name, description, interpreter, runs_on, body, checksum,
                         spec_json, source, created_at AS "created_at: DateTime<Utc>",
                         updated_at AS "updated_at: DateTime<Utc>""#,
            new.name,
            new.description,
            interpreter,
            runs_on,
            new.body,
            new.checksum,
            spec,
            new.source,
            now,
        )
        .fetch_one(&self.pool)
        .await?
        .try_into()
    }

    async fn get(&self, name: &str) -> Result<Option<Script>> {
        sqlx::query_as!(
            ScriptRow,
            r#"SELECT id AS "id!", name, description, interpreter, runs_on, body, checksum, spec_json,
                      source, created_at AS "created_at: DateTime<Utc>",
                      updated_at AS "updated_at: DateTime<Utc>"
               FROM scripts WHERE name = ?"#,
            name,
        )
        .fetch_optional(&self.pool)
        .await?
        .map(Script::try_from)
        .transpose()
    }

    async fn list(&self) -> Result<Vec<Script>> {
        sqlx::query_as!(
            ScriptRow,
            r#"SELECT id AS "id!", name, description, interpreter, runs_on, body, checksum, spec_json,
                      source, created_at AS "created_at: DateTime<Utc>",
                      updated_at AS "updated_at: DateTime<Utc>"
               FROM scripts ORDER BY name"#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Script::try_from)
        .collect()
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM scripts WHERE name = ?", name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn record(&self, script_id: i64, run: ScriptExecution<'_>) -> Result<()> {
        let result = run.result.as_str();
        let params = serde_json::to_string(run.params).expect("a map serializes");
        let exit_code = run.exit_code.map(i64::from);
        let now = Utc::now();
        sqlx::query!(
            r#"INSERT INTO script_runs (script_id, run_id, workload, from_workload, hook, checksum,
                                        params_json, result, exit_code, error, output_tail, ran_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            script_id,
            run.run_id,
            run.workload,
            run.from_workload,
            run.hook,
            run.checksum,
            params,
            result,
            exit_code,
            run.error,
            run.output_tail,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn history(
        &self,
        script: Option<&str>,
        workload: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ScriptRunRecord>> {
        let rows = sqlx::query!(
            r#"SELECT s.name AS "script!", r.workload, r.from_workload, r.hook, r.run_id,
                      r.checksum, r.params_json, r.result, r.exit_code, r.error, r.output_tail,
                      r.ran_at AS "ran_at: DateTime<Utc>"
               FROM script_runs r JOIN scripts s ON s.id = r.script_id
               WHERE (?1 IS NULL OR s.name = ?1)
                 AND (?2 IS NULL OR r.workload = ?2 OR r.from_workload = ?2)
               ORDER BY r.id DESC LIMIT ?3"#,
            script,
            workload,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let corrupt = |e: String| StateError::Corrupt(format!("script run: {e}"));
                Ok(ScriptRunRecord {
                    result: r
                        .result
                        .parse()
                        .map_err(|e: UnknownVariant| corrupt(e.to_string()))?,
                    params: serde_json::from_str::<BTreeMap<String, String>>(&r.params_json)
                        .map_err(|e| corrupt(e.to_string()))?,
                    exit_code: r.exit_code.and_then(|c| i32::try_from(c).ok()),
                    script: r.script,
                    workload: r.workload,
                    from_workload: r.from_workload,
                    hook: r.hook,
                    run_id: r.run_id,
                    checksum: r.checksum,
                    error: r.error,
                    output_tail: r.output_tail,
                    ran_at: r.ran_at,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ikit_core::{Interpreter, RunResult, RunsOn, ScriptParam};

    use super::*;
    use crate::{SqliteStore, StateStore};

    #[tokio::test]
    async fn scripts_upsert_and_keep_history() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = SqliteStore::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        let spec = ScriptSpec {
            params: vec![ScriptParam {
                name: "PGPASS".into(),
                from: Some("service.pg.env.POSTGRES_PASSWORD".parse().unwrap()),
                value: None,
                default: None,
                required: true,
                secret: false,
            }],
            ..Default::default()
        };
        let runs_on = RunsOn::Service("pg".into());
        let new = |body: &'static str, checksum: &'static str| NewScript {
            name: "create-db",
            description: "creates dbs",
            interpreter: Interpreter::Bash,
            runs_on: &runs_on,
            body,
            checksum,
            spec: &spec,
            source: Some("/repo/create-db.sh"),
        };
        let first = store.scripts().upsert(new("echo 1\n", "a")).await.unwrap();
        assert_eq!(first.spec, spec);
        assert_eq!(first.runs_on, runs_on);
        let second = store.scripts().upsert(new("echo 2\n", "b")).await.unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.created_at, first.created_at);
        assert_eq!(second.body, "echo 2\n");
        assert_eq!(store.scripts().list().await.unwrap().len(), 1);

        let params = BTreeMap::from([("PGPASS".to_owned(), "<secret>".to_owned())]);
        let run = |workload, from| ScriptExecution {
            run_id: None,
            workload,
            from_workload: from,
            hook: None,
            checksum: "b",
            params: &params,
            result: RunResult::Failed,
            exit_code: Some(2),
            error: Some("exit 2"),
            output_tail: Some("boom"),
        };
        store
            .scripts()
            .record(second.id, run("wl-dev-001", None))
            .await
            .unwrap();
        store
            .scripts()
            .record(second.id, run("wl-dev-002", Some("wl-prod-001")))
            .await
            .unwrap();
        let all = store.scripts().history(None, None, 10).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].from_workload.as_deref(), Some("wl-prod-001"));
        assert_eq!(all[1].exit_code, Some(2));
        assert_eq!(all[1].params, params);
        let prod = store
            .scripts()
            .history(Some("create-db"), Some("wl-prod-001"), 10)
            .await
            .unwrap();
        assert_eq!(prod.len(), 1);
        assert!(
            store
                .scripts()
                .history(Some("other"), None, 10)
                .await
                .unwrap()
                .is_empty()
        );

        assert!(store.scripts().delete("create-db").await.unwrap());
        assert!(store.scripts().get("create-db").await.unwrap().is_none());
        assert!(
            store
                .scripts()
                .history(None, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
