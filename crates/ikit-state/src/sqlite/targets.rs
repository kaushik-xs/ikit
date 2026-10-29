use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::target::{Check, TargetOrigin};
use ikit_core::{Connection, Target, TargetStatus};
use sqlx::SqlitePool;
use sqlx::types::Json;

use crate::{Result, StateError, TargetRepo, map_constraint};

pub(super) struct SqliteTargets {
    pool: SqlitePool,
}

impl SqliteTargets {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

struct TargetRow {
    id: i64,
    name: String,
    kind: String,
    origin: String,
    connection_json: Json<Connection>,
    status: String,
    last_checked_at: Option<DateTime<Utc>>,
    last_check_json: Json<Vec<Check>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<TargetRow> for Target {
    type Error = StateError;

    fn try_from(r: TargetRow) -> Result<Self> {
        let corrupt =
            |e: ikit_core::UnknownVariant| StateError::Corrupt(format!("target {}: {e}", r.name));
        Ok(Self {
            id: r.id,
            kind: r.kind.parse().map_err(corrupt)?,
            origin: r.origin.parse().map_err(corrupt)?,
            status: r.status.parse().map_err(corrupt)?,
            name: r.name,
            connection: r.connection_json.0,
            last_checked_at: r.last_checked_at,
            last_checks: r.last_check_json.0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

#[async_trait]
impl TargetRepo for SqliteTargets {
    async fn insert(
        &self,
        name: &str,
        origin: TargetOrigin,
        connection: &Connection,
    ) -> Result<Target> {
        let kind = connection.kind().as_str();
        let origin = origin.as_str();
        let json = Json(connection);
        let now = Utc::now();
        sqlx::query_as!(
            TargetRow,
            r#"INSERT INTO targets (name, kind, origin, connection_json, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!", name, kind, origin,
                         connection_json AS "connection_json: Json<Connection>", status,
                         last_checked_at AS "last_checked_at: DateTime<Utc>",
                         last_check_json AS "last_check_json: Json<Vec<Check>>",
                         created_at AS "created_at: DateTime<Utc>", updated_at AS "updated_at: DateTime<Utc>""#,
            name,
            kind,
            origin,
            json,
            now,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_constraint(format!("target {name}")))?
        .try_into()
    }

    async fn get(&self, name: &str) -> Result<Option<Target>> {
        sqlx::query_as!(
            TargetRow,
            r#"SELECT id AS "id!", name, kind, origin,
                      connection_json AS "connection_json: Json<Connection>", status,
                      last_checked_at AS "last_checked_at: DateTime<Utc>",
                      last_check_json AS "last_check_json: Json<Vec<Check>>",
                      created_at AS "created_at: DateTime<Utc>", updated_at AS "updated_at: DateTime<Utc>"
               FROM targets WHERE name = ?"#,
            name,
        )
        .fetch_optional(&self.pool)
        .await?
        .map(Target::try_from)
        .transpose()
    }

    async fn list(&self) -> Result<Vec<Target>> {
        sqlx::query_as!(
            TargetRow,
            r#"SELECT id AS "id!", name, kind, origin,
                      connection_json AS "connection_json: Json<Connection>", status,
                      last_checked_at AS "last_checked_at: DateTime<Utc>",
                      last_check_json AS "last_check_json: Json<Vec<Check>>",
                      created_at AS "created_at: DateTime<Utc>", updated_at AS "updated_at: DateTime<Utc>"
               FROM targets ORDER BY name"#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Target::try_from)
        .collect()
    }

    async fn record_checks(
        &self,
        name: &str,
        status: TargetStatus,
        checks: &[Check],
    ) -> Result<()> {
        let status = status.as_str();
        let checks = Json(checks);
        let now = Utc::now();
        let result = sqlx::query!(
            "UPDATE targets SET status = ?, last_check_json = ?, last_checked_at = ?, updated_at = ? WHERE name = ?",
            status,
            checks,
            now,
            now,
            name,
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StateError::NotFound(format!("target {name}")));
        }
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM targets WHERE name = ?", name)
            .execute(&self.pool)
            .await
            .map_err(map_constraint(format!("target {name}")))?;
        Ok(result.rows_affected() > 0)
    }
}
