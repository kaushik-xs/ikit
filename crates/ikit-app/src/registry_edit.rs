//! In-place edits of registry services and versions (`ikit registry set` / `version set`).

use std::collections::BTreeSet;

use ikit_core::registry::{EnvVar, Port, ServiceDef, VersionDef, Volume};
use ikit_core::{Category, Dependency, RegistryEntry, RegistryVersion};
use semver::Version;
use serde::Serialize;

use crate::services::split_env;
use crate::{App, AppError, Result};

/// Changes to a service's metadata. `None` leaves a field alone; `Some(None)` clears it.
#[derive(Debug, Clone, Default)]
pub struct ServiceChanges {
    pub display_name: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub owner: Option<Option<String>>,
    pub homepage: Option<Option<String>>,
    pub category: Option<Category>,
    /// Replaces all tags.
    pub tags: Option<Vec<String>>,
    pub add_tags: Vec<String>,
    pub remove_tags: Vec<String>,
}

impl ServiceChanges {
    pub fn is_empty(&self) -> bool {
        self.display_name.is_none()
            && self.description.is_none()
            && self.owner.is_none()
            && self.homepage.is_none()
            && self.category.is_none()
            && self.tags.is_none()
            && self.add_tags.is_empty()
            && self.remove_tags.is_empty()
    }
}

/// Changes to an existing version. Spec entries are upserted by key: env vars by name,
/// ports by container port, volumes by name, dependencies by service.
#[derive(Debug, Clone, Default)]
pub struct VersionChanges {
    pub image: Option<String>,
    /// `Some(None)` clears the digest.
    pub digest: Option<Option<String>>,
    pub release_notes: Option<Option<String>>,
    /// Hide from `latest` and ranges (pinned services keep working).
    pub deprecated: Option<bool>,
    pub ports: Vec<Port>,
    pub env: Vec<EnvVar>,
    pub volumes: Vec<Volume>,
    pub depends_on: Vec<Dependency>,
    pub remove_ports: Vec<u16>,
    pub remove_env: Vec<String>,
    pub remove_volumes: Vec<String>,
    pub remove_depends_on: Vec<String>,
}

impl VersionChanges {
    pub fn is_empty(&self) -> bool {
        self.image.is_none()
            && self.digest.is_none()
            && self.release_notes.is_none()
            && self.deprecated.is_none()
            && self.ports.is_empty()
            && self.env.is_empty()
            && self.volumes.is_empty()
            && self.depends_on.is_empty()
            && self.remove_ports.is_empty()
            && self.remove_env.is_empty()
            && self.remove_volumes.is_empty()
            && self.remove_depends_on.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionUpdate {
    pub version: RegistryVersion,
    /// Workload services (`workload/service`) running this version; their specs were rewritten.
    pub affected: Vec<String>,
}

fn nothing_to_change() -> AppError {
    AppError::Invalid("nothing to change; pass at least one option (see --help)".to_owned())
}

/// Inserts `item`, replacing an existing one with the same key.
fn upsert<T, K: PartialEq>(items: &mut Vec<T>, item: T, key: impl Fn(&T) -> K) {
    match items.iter().position(|i| key(i) == key(&item)) {
        Some(i) => items[i] = item,
        None => items.push(item),
    }
}

/// Removes the items with `keys`; every key must match something.
fn remove<T, K: PartialEq + std::fmt::Display>(
    items: &mut Vec<T>,
    keys: &[K],
    what: &str,
    key: impl Fn(&T) -> K,
) -> Result<()> {
    for k in keys {
        let before = items.len();
        items.retain(|i| key(i) != *k);
        if items.len() == before {
            return Err(AppError::Invalid(format!("no {what} `{k}` to remove")));
        }
    }
    Ok(())
}

impl App {
    pub async fn registry_update(
        &self,
        name: &str,
        changes: ServiceChanges,
    ) -> Result<RegistryEntry> {
        if changes.is_empty() {
            return Err(nothing_to_change());
        }
        let entry = self.registry_show(name).await?;
        let mut def = ServiceDef::from(&entry);
        let ServiceChanges {
            display_name,
            description,
            owner,
            homepage,
            category,
            tags,
            add_tags,
            remove_tags,
        } = changes;
        if let Some(v) = display_name {
            def.display_name = v;
        }
        if let Some(v) = description {
            def.description = v;
        }
        if let Some(v) = owner {
            def.owner = v;
        }
        if let Some(v) = homepage {
            def.homepage = v;
        }
        if let Some(v) = category {
            def.category = v;
        }
        if let Some(v) = tags {
            def.tags = v;
        }
        def.tags.extend(add_tags);
        def.tags
            .retain(|t| !remove_tags.contains(t) && !t.trim().is_empty());
        let mut seen = BTreeSet::new();
        def.tags.retain(|t| seen.insert(t.clone()));

        def.validate()?;
        self.store.registry().update_service(&def).await?;
        self.registry_show(name).await
    }

    /// Edits an existing version in place. Refused if the result would no longer accept the
    /// env values of a workload service running this version.
    pub async fn registry_version_update(
        &self,
        name: &str,
        version: &Version,
        changes: VersionChanges,
    ) -> Result<VersionUpdate> {
        if changes.is_empty() {
            return Err(nothing_to_change());
        }
        let entry = self.registry_show(name).await?;
        let stored = entry
            .version(version)
            .ok_or_else(|| AppError::NotFound(format!("{name}@{version}")))?;

        let mut def = VersionDef {
            digest: stored.digest.clone(),
            deprecated: stored.deprecated,
            release_notes: stored.release_notes.clone(),
            ..VersionDef::new(
                stored.version.clone(),
                stored.image.clone(),
                stored.spec.clone(),
            )
        };
        let c = changes;
        if let Some(image) = c.image {
            def.image = image;
        }
        if let Some(digest) = c.digest {
            def.digest = digest;
        }
        if let Some(notes) = c.release_notes {
            def.release_notes = notes;
        }
        if let Some(deprecated) = c.deprecated {
            def.deprecated = deprecated;
        }

        remove(&mut def.env, &c.remove_env, "env var", |e| e.name.clone())?;
        remove(&mut def.ports, &c.remove_ports, "port", |p| p.container)?;
        remove(&mut def.volumes, &c.remove_volumes, "volume", |v| {
            v.name.clone()
        })?;
        remove(
            &mut def.depends_on,
            &c.remove_depends_on,
            "dependency on",
            |d| d.name.clone(),
        )?;
        for mut var in c.env {
            // Keep schema details the flags cannot express.
            if let Some(existing) = def.env.iter().find(|e| e.name == var.name) {
                var.description = var.description.or_else(|| existing.description.clone());
                var.from = var.from.or_else(|| existing.from.clone());
                if var.allowed.is_empty() {
                    var.allowed = existing.allowed.clone();
                }
            }
            upsert(&mut def.env, var, |e| e.name.clone());
        }
        for p in c.ports {
            upsert(&mut def.ports, p, |p| p.container);
        }
        for v in c.volumes {
            upsert(&mut def.volumes, v, |v| v.name.clone());
        }
        for d in c.depends_on {
            upsert(&mut def.depends_on, d, |d| d.name.clone());
        }

        let mut check = ServiceDef::from(&entry);
        check.versions = vec![def.clone()];
        check.validate()?;

        // Every workload service on this version must still be valid under the new spec.
        let spec = def.spec();
        let mut affected = Vec::new();
        let mut broken = Vec::new();
        let mut workloads = BTreeSet::new();
        for usage in self
            .registry_usage(name)
            .await?
            .into_iter()
            .filter(|u| &u.version == version)
        {
            let services = self.services_list(&usage.workload).await?;
            let Some(instance) = services.iter().find(|s| s.name == usage.service) else {
                continue;
            };
            let mut env = instance.env.clone();
            for key in &instance.secret_env {
                env.insert(key.clone(), "<keychain>".to_owned());
            }
            let label = format!("{}/{}", usage.workload, usage.service);
            if let Err(e) = split_env(&usage.service, &spec, env) {
                broken.push(format!("{label}: {e}"));
            }
            affected.push(label);
            workloads.insert(usage.workload);
        }
        if !broken.is_empty() {
            return Err(AppError::Invalid(format!(
                "this change would break services running {name}@{version}:\n  {}\nAdd a new version instead, or fix them with `ikit svc set` first",
                broken.join("\n  ")
            )));
        }

        if !self.store.registry().update_version(name, &def).await? {
            return Err(AppError::NotFound(format!("{name}@{version}")));
        }
        for wl in &workloads {
            self.write_spec(wl).await?;
        }
        let version = self
            .registry_show(name)
            .await?
            .version(version)
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("{name}@{version}")))?;
        Ok(VersionUpdate { version, affected })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ikit_core::Connection;

    use super::*;
    use crate::ServiceRequest;
    use crate::testing::app;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[tokio::test]
    async fn update_service_metadata_and_tags() {
        let (_dir, app, _) = app().await;
        let changes = ServiceChanges {
            description: Some(Some("Postgres, the database".into())),
            homepage: Some(None),
            add_tags: vec!["oltp".into(), "sql".into()],
            remove_tags: vec!["database".into()],
            ..Default::default()
        };
        let entry = app.registry_update("postgres", changes).await.unwrap();
        assert_eq!(
            entry.service.description.as_deref(),
            Some("Postgres, the database")
        );
        assert_eq!(entry.service.homepage, None);
        assert_eq!(entry.service.tags, ["sql", "oltp"]);
        assert!(matches!(
            app.registry_update("postgres", ServiceChanges::default())
                .await,
            Err(AppError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn update_version_upserts_spec_entries() {
        let (_dir, app, _) = app().await;
        let changes = VersionChanges {
            image: Some("postgres:16.6-alpine".into()),
            env: vec![EnvVar {
                name: "POSTGRES_DB".into(),
                default: Some("main".into()),
                ..Default::default()
            }],
            ports: vec![Port {
                container: 5433,
                name: Some("alt".into()),
                protocol: Default::default(),
            }],
            remove_volumes: vec!["data".into()],
            ..Default::default()
        };
        let update = app
            .registry_version_update("postgres", &v("16.6.0"), changes)
            .await
            .unwrap();
        let spec = &update.version.spec;
        assert_eq!(update.version.image, "postgres:16.6-alpine");
        assert_eq!(
            spec.env_var("POSTGRES_DB").unwrap().default.as_deref(),
            Some("main")
        );
        assert_eq!(spec.env.len(), 3, "replaced in place, not duplicated");
        assert_eq!(spec.ports.len(), 2);
        assert!(spec.volumes.is_empty());

        let deprecate = VersionChanges {
            deprecated: Some(true),
            ..Default::default()
        };
        let update = app
            .registry_version_update("postgres", &v("16.6.0"), deprecate)
            .await
            .unwrap();
        assert!(update.version.deprecated);
        assert_eq!(
            update.version.image, "postgres:16.6-alpine",
            "other fields untouched"
        );

        let missing = VersionChanges {
            remove_env: vec!["NOPE".into()],
            ..Default::default()
        };
        assert!(matches!(
            app.registry_version_update("postgres", &v("16.6.0"), missing)
                .await,
            Err(AppError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn version_update_protects_running_services() {
        let (_dir, app, _) = app().await;
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        let env = BTreeMap::from([
            ("POSTGRES_PASSWORD".to_owned(), "pw".to_owned()),
            ("POSTGRES_DB".to_owned(), "orders".to_owned()),
        ]);
        let req = ServiceRequest {
            name: None,
            registry: "postgres".into(),
            constraint: "16".parse().unwrap(),
            group: Category::Data,
            env,
        };
        app.services_add("wl-acme-dev-001", vec![req])
            .await
            .unwrap();

        // Removing a var the service sets, or adding a new required one, is refused.
        let remove_db = VersionChanges {
            remove_env: vec!["POSTGRES_DB".into()],
            ..Default::default()
        };
        let err = app
            .registry_version_update("postgres", &v("16.6.0"), remove_db)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("wl-acme-dev-001/postgres"),
            "{err}"
        );
        let new_required = VersionChanges {
            env: vec![EnvVar {
                name: "PGTZ".into(),
                required: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(
            app.registry_version_update("postgres", &v("16.6.0"), new_required)
                .await
                .is_err()
        );

        // A compatible change goes through and rewrites the workload spec.
        let image = VersionChanges {
            image: Some("postgres:16.6-bookworm".into()),
            ..Default::default()
        };
        let update = app
            .registry_version_update("postgres", &v("16.6.0"), image)
            .await
            .unwrap();
        assert_eq!(update.affected, ["wl-acme-dev-001/postgres"]);
        let spec = std::fs::read_to_string(app.org().workload_spec("wl-acme-dev-001")).unwrap();
        assert!(spec.contains("postgres:16.6-bookworm"), "{spec}");
    }
}
