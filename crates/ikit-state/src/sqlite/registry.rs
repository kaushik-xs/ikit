use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ikit_core::registry::{
    ImportMode, ImportReport, RegistryLogin, ServiceDef, Source, Usage, VersionDef,
};
use ikit_core::{RegistryEntry, RegistryService, RegistryVersion, ServiceSpec};
use semver::Version;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};

use crate::{RegistryRepo, Result, StateError, map_constraint};

pub(super) struct SqliteRegistry {
    pool: SqlitePool,
}

impl SqliteRegistry {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

struct ServiceRow {
    id: i64,
    name: String,
    display_name: Option<String>,
    description: Option<String>,
    category: String,
    tags_json: Json<Vec<String>>,
    owner: Option<String>,
    homepage: Option<String>,
    source: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

struct VersionRow {
    id: i64,
    service_id: i64,
    version: String,
    image: String,
    digest: Option<String>,
    spec_json: Json<ServiceSpec>,
    release_notes: Option<String>,
    deprecated: bool,
    created_at: DateTime<Utc>,
}

fn corrupt(what: &str, e: impl std::fmt::Display) -> StateError {
    StateError::Corrupt(format!("{what}: {e}"))
}

impl TryFrom<ServiceRow> for RegistryService {
    type Error = StateError;

    fn try_from(r: ServiceRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            category: r.category.parse().map_err(|e| corrupt(&r.name, e))?,
            source: r.source.parse().map_err(|e| corrupt(&r.name, e))?,
            name: r.name,
            display_name: r.display_name,
            description: r.description,
            tags: r.tags_json.0,
            owner: r.owner,
            homepage: r.homepage,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

impl TryFrom<VersionRow> for RegistryVersion {
    type Error = StateError;

    fn try_from(r: VersionRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            version: Version::parse(&r.version).map_err(|e| corrupt(&r.version, e))?,
            image: r.image,
            digest: r.digest,
            spec: r.spec_json.0,
            release_notes: r.release_notes,
            deprecated: r.deprecated,
            created_at: r.created_at,
        })
    }
}

/// Loads one service (`name`) or all of them, with versions newest first.
async fn load(conn: &mut SqliteConnection, name: Option<&str>) -> Result<Vec<RegistryEntry>> {
    let services = sqlx::query_as!(
        ServiceRow,
        r#"SELECT id AS "id!", name, display_name, description, category,
                  tags_json AS "tags_json: Json<Vec<String>>", owner, homepage, source,
                  created_at AS "created_at: DateTime<Utc>", updated_at AS "updated_at: DateTime<Utc>"
           FROM registry_services WHERE ?1 IS NULL OR name = ?1 ORDER BY name"#,
        name,
    )
    .fetch_all(&mut *conn)
    .await?;

    let versions = sqlx::query_as!(
        VersionRow,
        r#"SELECT v.id AS "id!", v.service_id, v.version, v.image, v.digest,
                  v.spec_json AS "spec_json: Json<ServiceSpec>", v.release_notes,
                  v.deprecated AS "deprecated: bool", v.created_at AS "created_at: DateTime<Utc>"
           FROM registry_versions v JOIN registry_services s ON s.id = v.service_id
           WHERE ?1 IS NULL OR s.name = ?1"#,
        name,
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut by_service: BTreeMap<i64, Vec<RegistryVersion>> = BTreeMap::new();
    for row in versions {
        by_service
            .entry(row.service_id)
            .or_default()
            .push(row.try_into()?);
    }
    services
        .into_iter()
        .map(|row| {
            let mut versions = by_service.remove(&row.id).unwrap_or_default();
            versions.sort_by(|a, b| b.version.cmp(&a.version));
            Ok(RegistryEntry {
                service: row.try_into()?,
                versions,
            })
        })
        .collect()
}

async fn insert_service(
    conn: &mut SqliteConnection,
    def: &ServiceDef,
    source: Source,
) -> Result<i64> {
    let now = Utc::now();
    let category = def.category.as_str();
    let tags = Json(&def.tags);
    let source = source.as_str();
    let id = sqlx::query_scalar!(
        r#"INSERT INTO registry_services
               (name, display_name, description, category, tags_json, owner, homepage, source, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           RETURNING id AS "id!""#,
        def.name,
        def.display_name,
        def.description,
        category,
        tags,
        def.owner,
        def.homepage,
        source,
        now,
        now,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(map_constraint(format!("registry service {}", def.name)))?;
    for v in &def.versions {
        insert_version(conn, id, &def.name, v).await?;
    }
    Ok(id)
}

async fn insert_version(
    conn: &mut SqliteConnection,
    service_id: i64,
    service: &str,
    def: &VersionDef,
) -> Result<i64> {
    let now = Utc::now();
    let version = def.version.to_string();
    let spec = Json(def.spec());
    sqlx::query_scalar!(
        r#"INSERT INTO registry_versions
               (service_id, version, image, digest, spec_json, release_notes, deprecated, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)
           RETURNING id AS "id!""#,
        service_id,
        version,
        def.image,
        def.digest,
        spec,
        def.release_notes,
        def.deprecated,
        now,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(map_constraint(format!("{service}@{version}")))
}

async fn update_service(conn: &mut SqliteConnection, id: i64, def: &ServiceDef) -> Result<()> {
    let now = Utc::now();
    let category = def.category.as_str();
    let tags = Json(&def.tags);
    sqlx::query!(
        "UPDATE registry_services
         SET display_name = ?, description = ?, category = ?, tags_json = ?, owner = ?, homepage = ?, updated_at = ?
         WHERE id = ?",
        def.display_name,
        def.description,
        category,
        tags,
        def.owner,
        def.homepage,
        now,
        id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn update_version(conn: &mut SqliteConnection, id: i64, def: &VersionDef) -> Result<()> {
    let spec = Json(def.spec());
    sqlx::query!(
        "UPDATE registry_versions SET image = ?, digest = ?, spec_json = ?, release_notes = ?, deprecated = ? WHERE id = ?",
        def.image,
        def.digest,
        spec,
        def.release_notes,
        def.deprecated,
        id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn version_in_use(conn: &mut SqliteConnection, version_id: i64) -> Result<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM services WHERE registry_version_id = ?) AS "used!: bool""#,
        version_id
    )
    .fetch_one(&mut *conn)
    .await?)
}

async fn service_in_use(conn: &mut SqliteConnection, service_id: i64) -> Result<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM services WHERE registry_service_id = ?) AS "used!: bool""#,
        service_id
    )
    .fetch_one(&mut *conn)
    .await?)
}

/// Whether `def` would change the stored service metadata.
fn service_changed(existing: &RegistryService, def: &ServiceDef) -> bool {
    existing.display_name != def.display_name
        || existing.description != def.description
        || existing.category != def.category
        || existing.tags != def.tags
        || existing.owner != def.owner
        || existing.homepage != def.homepage
}

fn version_changed(existing: &RegistryVersion, def: &VersionDef) -> bool {
    existing.image != def.image
        || existing.digest != def.digest
        || existing.spec != def.spec()
        || existing.release_notes != def.release_notes
        || existing.deprecated != def.deprecated
}

#[async_trait]
impl RegistryRepo for SqliteRegistry {
    async fn list(&self) -> Result<Vec<RegistryEntry>> {
        load(&mut *self.pool.acquire().await?, None).await
    }

    async fn get(&self, name: &str) -> Result<Option<RegistryEntry>> {
        Ok(load(&mut *self.pool.acquire().await?, Some(name))
            .await?
            .pop())
    }

    async fn add_service(&self, def: &ServiceDef, source: Source) -> Result<RegistryEntry> {
        let mut tx = self.pool.begin().await?;
        insert_service(&mut tx, def, source).await?;
        let entry = load(&mut tx, Some(&def.name)).await?.pop();
        tx.commit().await?;
        entry.ok_or_else(|| StateError::Corrupt(format!("registry service {} vanished", def.name)))
    }

    async fn add_version(&self, service: &str, def: &VersionDef) -> Result<RegistryVersion> {
        let mut tx = self.pool.begin().await?;
        let service_id = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM registry_services WHERE name = ?"#,
            service
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StateError::NotFound(format!("registry service {service}")))?;
        let id = insert_version(&mut tx, service_id, service, def).await?;
        let now = Utc::now();
        sqlx::query!(
            "UPDATE registry_services SET updated_at = ? WHERE id = ?",
            now,
            service_id
        )
        .execute(&mut *tx)
        .await?;
        let entry = load(&mut tx, Some(service)).await?.pop();
        tx.commit().await?;
        entry
            .and_then(|e| e.versions.into_iter().find(|v| v.id == id))
            .ok_or_else(|| StateError::Corrupt(format!("{service}@{} vanished", def.version)))
    }

    async fn set_deprecated(
        &self,
        service: &str,
        version: &Version,
        deprecated: bool,
    ) -> Result<bool> {
        let version = version.to_string();
        let result = sqlx::query!(
            "UPDATE registry_versions SET deprecated = ?
             WHERE version = ? AND service_id = (SELECT id FROM registry_services WHERE name = ?)",
            deprecated,
            version,
            service,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn update_service(&self, def: &ServiceDef) -> Result<bool> {
        let mut conn = self.pool.acquire().await?;
        let id = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM registry_services WHERE name = ?"#,
            def.name
        )
        .fetch_optional(&mut *conn)
        .await?;
        match id {
            Some(id) => update_service(&mut conn, id, def).await.map(|()| true),
            None => Ok(false),
        }
    }

    async fn update_version(&self, service: &str, def: &VersionDef) -> Result<bool> {
        let mut conn = self.pool.acquire().await?;
        let version = def.version.to_string();
        let id = sqlx::query_scalar!(
            r#"SELECT v.id AS "id!" FROM registry_versions v
               JOIN registry_services s ON s.id = v.service_id
               WHERE s.name = ? AND v.version = ?"#,
            service,
            version,
        )
        .fetch_optional(&mut *conn)
        .await?;
        match id {
            Some(id) => update_version(&mut conn, id, def).await.map(|()| true),
            None => Ok(false),
        }
    }

    async fn delete_service(&self, name: &str) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM registry_services WHERE name = ?", name)
            .execute(&self.pool)
            .await
            .map_err(map_constraint(format!("registry service {name}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn delete_version(&self, service: &str, version: &Version) -> Result<bool> {
        let version = version.to_string();
        let result = sqlx::query!(
            "DELETE FROM registry_versions
             WHERE version = ? AND service_id = (SELECT id FROM registry_services WHERE name = ?)",
            version,
            service,
        )
        .execute(&self.pool)
        .await
        .map_err(map_constraint(format!("{service}@{version}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn import(
        &self,
        defs: &[ServiceDef],
        mode: ImportMode,
        source: Source,
    ) -> Result<ImportReport> {
        let mut tx = self.pool.begin().await?;
        let existing: BTreeMap<String, RegistryEntry> = load(&mut tx, None)
            .await?
            .into_iter()
            .map(|e| (e.service.name.clone(), e))
            .collect();
        let mut report = ImportReport::default();

        for def in defs {
            let Some(entry) = existing.get(&def.name) else {
                insert_service(&mut tx, def, source).await?;
                report.services_added.push(def.name.clone());
                report.versions_added.extend(
                    def.versions
                        .iter()
                        .map(|v| format!("{}@{}", def.name, v.version)),
                );
                continue;
            };

            if mode != ImportMode::InsertMissing && service_changed(&entry.service, def) {
                update_service(&mut tx, entry.service.id, def).await?;
                report.services_updated.push(def.name.clone());
            }
            for v in &def.versions {
                let label = format!("{}@{}", def.name, v.version);
                match entry.version(&v.version) {
                    None => {
                        insert_version(&mut tx, entry.service.id, &def.name, v).await?;
                        report.versions_added.push(label);
                    }
                    Some(stored)
                        if mode != ImportMode::InsertMissing && version_changed(stored, v) =>
                    {
                        update_version(&mut tx, stored.id, v).await?;
                        report.versions_updated.push(label);
                    }
                    Some(_) => {}
                }
            }
            if mode == ImportMode::Replace {
                let wanted: BTreeSet<&Version> = def.versions.iter().map(|v| &v.version).collect();
                for stored in entry
                    .versions
                    .iter()
                    .filter(|v| !wanted.contains(&v.version))
                {
                    let label = format!("{}@{}", def.name, stored.version);
                    if version_in_use(&mut tx, stored.id).await? {
                        report.kept_in_use.push(label);
                    } else {
                        sqlx::query!("DELETE FROM registry_versions WHERE id = ?", stored.id)
                            .execute(&mut *tx)
                            .await?;
                        report.versions_removed.push(label);
                    }
                }
            }
        }

        if mode == ImportMode::Replace {
            let wanted: BTreeSet<&str> = defs.iter().map(|d| d.name.as_str()).collect();
            for entry in existing
                .values()
                .filter(|e| !wanted.contains(e.service.name.as_str()))
            {
                if service_in_use(&mut tx, entry.service.id).await? {
                    report.kept_in_use.push(entry.service.name.clone());
                } else {
                    sqlx::query!(
                        "DELETE FROM registry_services WHERE id = ?",
                        entry.service.id
                    )
                    .execute(&mut *tx)
                    .await?;
                    report.services_removed.push(entry.service.name.clone());
                }
            }
        }

        tx.commit().await?;
        Ok(report)
    }

    async fn usage(&self, name: &str) -> Result<Vec<Usage>> {
        let rows = sqlx::query!(
            r#"SELECT w.name AS "workload!", s.name AS "service!", v.version AS "version!"
               FROM services s
               JOIN workloads w ON w.id = s.workload_id
               JOIN registry_services r ON r.id = s.registry_service_id
               JOIN registry_versions v ON v.id = s.registry_version_id
               WHERE r.name = ?
               ORDER BY w.name, s.name"#,
            name,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(Usage {
                    version: Version::parse(&r.version).map_err(|e| corrupt(&r.version, e))?,
                    workload: r.workload,
                    service: r.service,
                })
            })
            .collect()
    }

    async fn logins(&self) -> Result<Vec<RegistryLogin>> {
        Ok(sqlx::query_as!(
            RegistryLogin,
            r#"SELECT host AS "host!", username, created_at AS "created_at: DateTime<Utc>"
               FROM registry_logins ORDER BY host"#
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn set_login(&self, host: &str, username: &str) -> Result<RegistryLogin> {
        let now = Utc::now();
        Ok(sqlx::query_as!(
            RegistryLogin,
            r#"INSERT INTO registry_logins (host, username, created_at) VALUES (?, ?, ?)
               ON CONFLICT (host) DO UPDATE SET username = excluded.username, created_at = excluded.created_at
               RETURNING host AS "host!", username, created_at AS "created_at: DateTime<Utc>""#,
            host,
            username,
            now,
        )
        .fetch_one(&self.pool)
        .await?)
    }

    async fn delete_login(&self, host: &str) -> Result<bool> {
        let result = sqlx::query!("DELETE FROM registry_logins WHERE host = ?", host)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use ikit_core::Category;

    use super::*;
    use crate::{SqliteStore, StateStore};

    async fn store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = SqliteStore::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        (dir, store)
    }

    fn def(name: &str, versions: &[&str]) -> ServiceDef {
        ServiceDef {
            name: name.to_owned(),
            display_name: None,
            description: None,
            category: Category::Data,
            tags: vec![],
            owner: None,
            homepage: None,
            versions: versions
                .iter()
                .map(|v| {
                    let version = ikit_core::version::parse_version(v).unwrap();
                    VersionDef::new(version, format!("{name}:{v}"), ServiceSpec::default())
                })
                .collect(),
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[tokio::test]
    async fn add_get_and_versions_sorted_newest_first() {
        let (_dir, store) = store().await;
        let repo = store.registry();
        repo.add_service(
            &def("postgres", &["16.4.0", "17.2.0", "16.10.0"]),
            Source::Manual,
        )
        .await
        .unwrap();

        let entry = repo.get("postgres").await.unwrap().unwrap();
        let versions: Vec<String> = entry
            .versions
            .iter()
            .map(|v| v.version.to_string())
            .collect();
        assert_eq!(versions, ["17.2.0", "16.10.0", "16.4.0"]);
        assert!(matches!(
            repo.add_service(&def("postgres", &["1.0.0"]), Source::Manual)
                .await,
            Err(StateError::Conflict(_))
        ));

        let added = repo
            .add_version(
                "postgres",
                &VersionDef::new(v("18.0.0"), "postgres:18".into(), ServiceSpec::default()),
            )
            .await
            .unwrap();
        assert_eq!(added.version, v("18.0.0"));
        assert!(matches!(
            repo.add_version(
                "postgres",
                &VersionDef::new(v("18.0.0"), "x".into(), ServiceSpec::default())
            )
            .await,
            Err(StateError::Conflict(_))
        ));
        assert!(matches!(
            repo.add_version(
                "nope",
                &VersionDef::new(v("1.0.0"), "x".into(), ServiceSpec::default())
            )
            .await,
            Err(StateError::NotFound(_))
        ));

        assert!(
            repo.set_deprecated("postgres", &v("16.4.0"), true)
                .await
                .unwrap()
        );
        assert!(
            repo.get("postgres")
                .await
                .unwrap()
                .unwrap()
                .version(&v("16.4.0"))
                .unwrap()
                .deprecated
        );
    }

    #[tokio::test]
    async fn import_modes() {
        let (_dir, store) = store().await;
        let repo = store.registry();
        repo.import(
            &[def("postgres", &["16.4.0"]), def("redis", &["7.4.1"])],
            ImportMode::Merge,
            Source::Import,
        )
        .await
        .unwrap();

        // Insert-missing adds versions but leaves existing entries alone.
        let mut changed = def("postgres", &["16.4.0", "17.2.0"]);
        changed.versions[0].image = "changed:16".into();
        changed.description = Some("changed".into());
        let report = repo
            .import(&[changed.clone()], ImportMode::InsertMissing, Source::Seed)
            .await
            .unwrap();
        assert_eq!(report.versions_added, ["postgres@17.2.0"]);
        assert!(report.services_updated.is_empty() && report.versions_updated.is_empty());
        let pg = repo.get("postgres").await.unwrap().unwrap();
        assert_eq!(pg.version(&v("16.4.0")).unwrap().image, "postgres:16.4.0");

        // Merge updates.
        let report = repo
            .import(&[changed], ImportMode::Merge, Source::Import)
            .await
            .unwrap();
        assert_eq!(report.services_updated, ["postgres"]);
        assert_eq!(report.versions_updated, ["postgres@16.4.0"]);

        // Replace removes what the file no longer lists.
        let report = repo
            .import(
                &[def("postgres", &["17.2.0"])],
                ImportMode::Replace,
                Source::Import,
            )
            .await
            .unwrap();
        assert_eq!(report.versions_removed, ["postgres@16.4.0"]);
        assert_eq!(report.services_removed, ["redis"]);
        assert_eq!(repo.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_service_and_version() {
        let (_dir, store) = store().await;
        let repo = store.registry();
        repo.add_service(&def("postgres", &["16.4.0"]), Source::Manual)
            .await
            .unwrap();

        let mut changed = def("postgres", &[]);
        changed.description = Some("Relational database".into());
        changed.tags = vec!["sql".into()];
        assert!(repo.update_service(&changed).await.unwrap());
        assert!(!repo.update_service(&def("nope", &[])).await.unwrap());

        let mut version = VersionDef::new(
            v("16.4.0"),
            "postgres:16.4-alpine".into(),
            ServiceSpec::default(),
        );
        version.digest = Some("sha256:abc".into());
        assert!(repo.update_version("postgres", &version).await.unwrap());
        version.version = v("9.9.9");
        assert!(!repo.update_version("postgres", &version).await.unwrap());

        let entry = repo.get("postgres").await.unwrap().unwrap();
        assert_eq!(
            entry.service.description.as_deref(),
            Some("Relational database")
        );
        assert_eq!(entry.service.tags, ["sql"]);
        assert_eq!(entry.versions[0].image, "postgres:16.4-alpine");
        assert_eq!(entry.versions[0].digest.as_deref(), Some("sha256:abc"));
    }
}
