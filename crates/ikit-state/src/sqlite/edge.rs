use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::{AutoRoutes, Redirect, Route, UnknownVariant};
use sqlx::SqlitePool;
use sqlx::types::Json;

use crate::{EdgeRepo, NewRedirect, NewRoute, Result, StateError, map_constraint};

pub(super) struct SqliteEdge {
    pool: SqlitePool,
}

impl SqliteEdge {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

struct RouteRow {
    id: i64,
    workload: String,
    service: String,
    port: i64,
    host: String,
    path: String,
    tls: String,
    upstream_port: i64,
    origin: String,
    created_at: DateTime<Utc>,
}

impl RouteRow {
    fn into_route(self) -> Result<(String, Route)> {
        let key = ikit_core::edge::route_key(&self.host, Some(&self.path));
        let corrupt = |e: UnknownVariant| StateError::Corrupt(format!("route {key}: {e}"));
        let port = |p: i64| {
            u16::try_from(p).map_err(|_| StateError::Corrupt(format!("route {key}: port {p}")))
        };
        let route = Route {
            id: self.id,
            service: self.service,
            port: port(self.port)?,
            host: self.host,
            path: (!self.path.is_empty()).then_some(self.path),
            tls: self.tls.parse().map_err(corrupt)?,
            upstream_port: port(self.upstream_port)?,
            origin: self.origin.parse().map_err(corrupt)?,
            created_at: self.created_at,
        };
        Ok((self.workload, route))
    }
}

struct RedirectRow {
    id: i64,
    workload: String,
    host: String,
    to_url: String,
    permanent: bool,
    tls: String,
    created_at: DateTime<Utc>,
}

impl RedirectRow {
    fn into_redirect(self) -> Result<(String, Redirect)> {
        let redirect = Redirect {
            id: self.id,
            tls: self.tls.parse().map_err(|e: UnknownVariant| {
                StateError::Corrupt(format!("redirect {}: {e}", self.host))
            })?,
            host: self.host,
            to: self.to_url,
            permanent: self.permanent,
            created_at: self.created_at,
        };
        Ok((self.workload, redirect))
    }
}

#[async_trait]
impl EdgeRepo for SqliteEdge {
    async fn routes(&self, workload_id: i64) -> Result<Vec<Route>> {
        sqlx::query_as!(
            RouteRow,
            r#"SELECT r.id AS "id!", w.name AS workload, r.service, r.port, r.host, r.path, r.tls,
                      r.upstream_port, r.origin, r.created_at AS "created_at: DateTime<Utc>"
               FROM routes r JOIN workloads w ON w.id = r.workload_id
               WHERE r.workload_id = ? ORDER BY r.host, r.path"#,
            workload_id,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| Ok(r.into_route()?.1))
        .collect()
    }

    async fn all_routes(&self) -> Result<Vec<(String, Route)>> {
        sqlx::query_as!(
            RouteRow,
            r#"SELECT r.id AS "id!", w.name AS workload, r.service, r.port, r.host, r.path, r.tls,
                      r.upstream_port, r.origin, r.created_at AS "created_at: DateTime<Utc>"
               FROM routes r JOIN workloads w ON w.id = r.workload_id
               ORDER BY w.name, r.host, r.path"#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(RouteRow::into_route)
        .collect()
    }

    async fn insert_route(&self, workload_id: i64, new: NewRoute<'_>) -> Result<Route> {
        let path = new.path.unwrap_or("");
        let tls = new.tls.as_str();
        let origin = new.origin.as_str();
        let now = Utc::now();
        let id = sqlx::query_scalar!(
            r#"INSERT INTO routes
                 (workload_id, service, port, host, path, tls, upstream_port, origin, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id AS "id!""#,
            workload_id,
            new.service,
            new.port,
            new.host,
            path,
            tls,
            new.upstream_port,
            origin,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_constraint(format!(
            "route {}",
            ikit_core::edge::route_key(new.host, new.path)
        )))?;
        Ok(Route {
            id,
            service: new.service.to_owned(),
            port: new.port,
            host: new.host.to_owned(),
            path: new.path.map(str::to_owned),
            tls: new.tls,
            upstream_port: new.upstream_port,
            origin: new.origin,
            created_at: now,
        })
    }

    async fn delete_route(&self, workload_id: i64, host: &str, path: Option<&str>) -> Result<bool> {
        let path = path.unwrap_or("");
        let result = sqlx::query!(
            "DELETE FROM routes WHERE workload_id = ? AND host = ? AND path = ?",
            workload_id,
            host,
            path,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn delete_service_routes(&self, workload_id: i64, service: &str) -> Result<u64> {
        let result = sqlx::query!(
            "DELETE FROM routes WHERE workload_id = ? AND service = ?",
            workload_id,
            service,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn redirects(&self, workload_id: i64) -> Result<Vec<Redirect>> {
        sqlx::query_as!(
            RedirectRow,
            r#"SELECT r.id AS "id!", w.name AS workload, r.host, r.to_url,
                      r.permanent AS "permanent: bool", r.tls,
                      r.created_at AS "created_at: DateTime<Utc>"
               FROM redirects r JOIN workloads w ON w.id = r.workload_id
               WHERE r.workload_id = ? ORDER BY r.host"#,
            workload_id,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| Ok(r.into_redirect()?.1))
        .collect()
    }

    async fn all_redirects(&self) -> Result<Vec<(String, Redirect)>> {
        sqlx::query_as!(
            RedirectRow,
            r#"SELECT r.id AS "id!", w.name AS workload, r.host, r.to_url,
                      r.permanent AS "permanent: bool", r.tls,
                      r.created_at AS "created_at: DateTime<Utc>"
               FROM redirects r JOIN workloads w ON w.id = r.workload_id
               ORDER BY w.name, r.host"#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(RedirectRow::into_redirect)
        .collect()
    }

    async fn insert_redirect(&self, workload_id: i64, new: NewRedirect<'_>) -> Result<Redirect> {
        let tls = new.tls.as_str();
        let now = Utc::now();
        let id = sqlx::query_scalar!(
            r#"INSERT INTO redirects (workload_id, host, to_url, permanent, tls, created_at)
               VALUES (?, ?, ?, ?, ?, ?) RETURNING id AS "id!""#,
            workload_id,
            new.host,
            new.to,
            new.permanent,
            tls,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_constraint(format!("redirect {}", new.host)))?;
        Ok(Redirect {
            id,
            host: new.host.to_owned(),
            to: new.to.to_owned(),
            permanent: new.permanent,
            tls: new.tls,
            created_at: now,
        })
    }

    async fn delete_redirect(&self, workload_id: i64, host: &str) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM redirects WHERE workload_id = ? AND host = ?",
            workload_id,
            host,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn auto_routes(&self, workload_id: i64) -> Result<Option<AutoRoutes>> {
        let row = sqlx::query_scalar!(
            r#"SELECT config_json AS "config_json: Json<AutoRoutes>"
               FROM auto_routes WHERE workload_id = ?"#,
            workload_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|j| j.0))
    }

    async fn set_auto_routes(&self, workload_id: i64, auto: Option<&AutoRoutes>) -> Result<()> {
        match auto {
            Some(auto) => {
                let json = Json(auto);
                let now = Utc::now();
                sqlx::query!(
                    r#"INSERT INTO auto_routes (workload_id, config_json, updated_at)
                       VALUES (?, ?, ?)
                       ON CONFLICT (workload_id) DO UPDATE
                       SET config_json = excluded.config_json, updated_at = excluded.updated_at"#,
                    workload_id,
                    json,
                    now,
                )
                .execute(&self.pool)
                .await?;
            }
            None => {
                sqlx::query!("DELETE FROM auto_routes WHERE workload_id = ?", workload_id)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ikit_core::target::TargetOrigin;
    use ikit_core::{AcmeCa, Category, Connection, EdgeConfig, EdgeKind, RouteOrigin, TlsMode};

    use super::*;
    use crate::{SqliteStore, StateStore};

    #[tokio::test]
    async fn routes_redirects_and_auto_routes() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = SqliteStore::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        let target = store
            .targets()
            .insert("box", TargetOrigin::Attached, &Connection::Local)
            .await
            .unwrap();
        assert_eq!(target.edge, None);
        let edge = EdgeConfig {
            kind: EdgeKind::Caddy,
            email: Some("ops@acme.com".into()),
            ca: AcmeCa::Staging,
            version: None,
        };
        store.targets().set_edge("box", Some(&edge)).await.unwrap();
        assert_eq!(
            store.targets().get("box").await.unwrap().unwrap().edge,
            Some(edge)
        );
        let wl = store
            .workloads()
            .create("wl-acme", "dev", target.id)
            .await
            .unwrap();

        let new = NewRoute {
            service: "web",
            port: 8080,
            host: "web.acme.dev",
            path: None,
            tls: TlsMode::Acme,
            upstream_port: 20000,
            origin: RouteOrigin::Auto,
        };
        store.edge().insert_route(wl.id, new).await.unwrap();
        assert!(matches!(
            store.edge().insert_route(wl.id, new).await,
            Err(StateError::Conflict(_))
        ));
        store
            .edge()
            .insert_route(
                wl.id,
                NewRoute {
                    path: Some("/api"),
                    service: "api",
                    origin: RouteOrigin::Manual,
                    upstream_port: 20001,
                    ..new
                },
            )
            .await
            .unwrap();
        let routes = store.edge().routes(wl.id).await.unwrap();
        assert_eq!(
            routes.iter().map(Route::key).collect::<Vec<_>>(),
            ["web.acme.dev", "web.acme.dev/api"]
        );
        assert_eq!(routes[1].origin, RouteOrigin::Manual);
        let all = store.edge().all_routes().await.unwrap();
        assert_eq!(all[0].0, wl.name);

        assert_eq!(
            store
                .edge()
                .delete_service_routes(wl.id, "api")
                .await
                .unwrap(),
            1
        );
        assert!(
            store
                .edge()
                .delete_route(wl.id, "web.acme.dev", None)
                .await
                .unwrap()
        );
        assert!(store.edge().routes(wl.id).await.unwrap().is_empty());

        let r = NewRedirect {
            host: "www.acme.dev",
            to: "https://acme.dev",
            permanent: true,
            tls: TlsMode::Acme,
        };
        store.edge().insert_redirect(wl.id, r).await.unwrap();
        assert!(store.edge().insert_redirect(wl.id, r).await.is_err());
        assert_eq!(store.edge().all_redirects().await.unwrap().len(), 1);
        assert!(
            store
                .edge()
                .delete_redirect(wl.id, "www.acme.dev")
                .await
                .unwrap()
        );

        let auto = AutoRoutes {
            groups: vec![Category::App],
            port_name: "http".into(),
            excluded: vec![],
        };
        store
            .edge()
            .set_auto_routes(wl.id, Some(&auto))
            .await
            .unwrap();
        assert_eq!(store.edge().auto_routes(wl.id).await.unwrap(), Some(auto));
        store.edge().set_auto_routes(wl.id, None).await.unwrap();
        assert_eq!(store.edge().auto_routes(wl.id).await.unwrap(), None);
    }
}
