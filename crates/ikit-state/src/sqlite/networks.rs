use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::{Network, UnknownVariant};
use sqlx::SqlitePool;

use crate::{NetworkRepo, NewNetwork, Result, StateError, map_constraint};

pub(super) struct SqliteNetworks {
    pool: SqlitePool,
}

impl SqliteNetworks {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Linked workload names per network row id.
    async fn links(&self) -> Result<BTreeMap<i64, Vec<String>>> {
        let rows = sqlx::query!(
            r#"SELECT wn.network_id AS "network_id!", w.name
               FROM workload_networks wn JOIN workloads w ON w.id = wn.workload_id
               ORDER BY w.name"#
        )
        .fetch_all(&self.pool)
        .await?;
        let mut links: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        for r in rows {
            links.entry(r.network_id).or_default().push(r.name);
        }
        Ok(links)
    }

    async fn with_links(&self, rows: Vec<NetworkRow>) -> Result<Vec<Network>> {
        let mut links = self.links().await?;
        rows.into_iter()
            .map(|r| {
                let workloads = links.remove(&r.id).unwrap_or_default();
                r.into_network(workloads)
            })
            .collect()
    }
}

struct NetworkRow {
    id: i64,
    name: String,
    network_id: String,
    profile: String,
    origin: String,
    cidr: Option<String>,
    created_at: DateTime<Utc>,
}

impl NetworkRow {
    fn into_network(self, workloads: Vec<String>) -> Result<Network> {
        Ok(Network {
            id: self.id,
            origin: self.origin.parse().map_err(|e: UnknownVariant| {
                StateError::Corrupt(format!("network {}: {e}", self.name))
            })?,
            name: self.name,
            network_id: self.network_id,
            profile: self.profile,
            cidr: self.cidr,
            workloads,
            created_at: self.created_at,
        })
    }
}

#[async_trait]
impl NetworkRepo for SqliteNetworks {
    async fn insert(&self, new: NewNetwork<'_>) -> Result<Network> {
        let origin = new.origin.as_str();
        let now = Utc::now();
        sqlx::query_as!(
            NetworkRow,
            r#"INSERT INTO networks (name, network_id, profile, origin, cidr, created_at)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!", name, network_id, profile, origin, cidr,
                         created_at AS "created_at: DateTime<Utc>""#,
            new.name,
            new.network_id,
            new.profile,
            origin,
            new.cidr,
            now,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(map_constraint(format!(
            "network {} ({})",
            new.name, new.network_id
        )))?
        .into_network(vec![])
    }

    async fn get(&self, name: &str) -> Result<Option<Network>> {
        let row = sqlx::query_as!(
            NetworkRow,
            r#"SELECT id AS "id!", name, network_id, profile, origin, cidr,
                      created_at AS "created_at: DateTime<Utc>"
               FROM networks WHERE name = ?"#,
            name,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some(r) => self.with_links(vec![r]).await?.pop(),
            None => None,
        })
    }

    async fn list(&self) -> Result<Vec<Network>> {
        let rows = sqlx::query_as!(
            NetworkRow,
            r#"SELECT id AS "id!", name, network_id, profile, origin, cidr,
                      created_at AS "created_at: DateTime<Utc>"
               FROM networks ORDER BY name"#,
        )
        .fetch_all(&self.pool)
        .await?;
        self.with_links(rows).await
    }

    async fn delete(&self, name: &str) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM networks WHERE name = ?", name)
            .execute(&self.pool)
            .await
            .map_err(map_constraint(format!("network {name}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn link(&self, workload_id: i64, network_id: i64) -> Result<()> {
        sqlx::query!(
            r#"INSERT INTO workload_networks (workload_id, network_id) VALUES (?, ?)
               ON CONFLICT (workload_id) DO UPDATE SET network_id = excluded.network_id"#,
            workload_id,
            network_id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn unlink(&self, workload_id: i64) -> Result<bool> {
        let result = sqlx::query!(
            "DELETE FROM workload_networks WHERE workload_id = ?",
            workload_id
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn for_workload(&self, workload_id: i64) -> Result<Option<Network>> {
        let row = sqlx::query_as!(
            NetworkRow,
            r#"SELECT n.id AS "id!", n.name, n.network_id, n.profile, n.origin, n.cidr,
                      n.created_at AS "created_at: DateTime<Utc>"
               FROM networks n JOIN workload_networks wn ON wn.network_id = n.id
               WHERE wn.workload_id = ?"#,
            workload_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some(r) => self.with_links(vec![r]).await?.pop(),
            None => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use ikit_core::target::TargetOrigin;
    use ikit_core::{Connection, NetworkOrigin};

    use super::*;
    use crate::{SqliteStore, StateStore};

    #[tokio::test]
    async fn networks_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = SqliteStore::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        let new = NewNetwork {
            name: "acme",
            network_id: "8056c2e21c000001",
            profile: "zt",
            origin: NetworkOrigin::Created,
            cidr: Some("10.147.17.0/24"),
        };
        let n = store.networks().insert(new).await.unwrap();
        assert!(n.workloads.is_empty());
        assert!(matches!(
            store
                .networks()
                .insert(NewNetwork {
                    name: "other",
                    ..new
                })
                .await,
            Err(StateError::Conflict(_))
        ));
        let other = store
            .networks()
            .insert(NewNetwork {
                name: "other",
                network_id: "8056c2e21c000002",
                origin: NetworkOrigin::Attached,
                cidr: None,
                ..new
            })
            .await
            .unwrap();

        let target = store
            .targets()
            .insert("laptop", TargetOrigin::Attached, &Connection::Local)
            .await
            .unwrap();
        let wl = store
            .workloads()
            .create("wl", "dev", target.id)
            .await
            .unwrap();
        store.networks().link(wl.id, n.id).await.unwrap();
        assert_eq!(
            store
                .networks()
                .get("acme")
                .await
                .unwrap()
                .unwrap()
                .workloads,
            ["wl-dev-001"]
        );
        assert!(matches!(
            store.networks().delete("acme").await,
            Err(StateError::InUse(_))
        ));

        // Linking again moves the workload.
        store.networks().link(wl.id, other.id).await.unwrap();
        let linked = store.networks().for_workload(wl.id).await.unwrap().unwrap();
        assert_eq!(linked.name, "other");
        assert!(
            store.networks().list().await.unwrap()[0]
                .workloads
                .is_empty()
        );

        assert!(store.networks().unlink(wl.id).await.unwrap());
        assert!(!store.networks().unlink(wl.id).await.unwrap());
        assert!(store.networks().delete("other").await.unwrap());
        assert!(
            store
                .networks()
                .for_workload(wl.id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
