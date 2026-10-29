use ikit_core::registry::{
    ImportMode, ImportReport, RegistryFile, ServiceDef, Source, Usage, VersionDef,
};
use ikit_core::{Category, RegistryEntry, RegistryVersion, ServiceSpec, VersionConstraint};
use semver::Version;

use crate::{App, AppError, Result};

/// Built-in services imported by `ikit init`.
pub(crate) const SEEDS: &str = include_str!("../seeds/registry.yaml");

pub(crate) fn seeds() -> RegistryFile {
    let file: RegistryFile = serde_yaml_ng::from_str(SEEDS).expect("built-in seeds parse");
    for s in &file.services {
        s.validate().expect("built-in seeds are valid");
    }
    file
}

/// A workload service running an older version than the registry offers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Outdated {
    pub workload: String,
    pub service: String,
    pub registry_service: String,
    pub constraint: VersionConstraint,
    pub current: Version,
    pub deprecated: bool,
    /// Newest version its constraint allows, when newer than `current`
    /// (`ikit svc upgrade` moves to it).
    pub wanted: Option<Version>,
    /// Newest non-deprecated version in the registry, when newer than `current`.
    pub latest: Option<Version>,
}

#[derive(Debug, Clone, Default)]
pub struct RegistryFilter {
    pub category: Option<Category>,
    pub tag: Option<String>,
}

/// A new version for an existing registry service.
#[derive(Debug, Clone)]
pub struct AddVersion {
    pub version: Version,
    pub image: String,
    pub digest: Option<String>,
    pub release_notes: Option<String>,
    /// The version's spec; `None` copies it from `copy_from`, or from the newest version.
    pub spec: Option<ServiceSpec>,
    pub copy_from: Option<Version>,
}

fn not_found(name: &str) -> AppError {
    AppError::NotFound(format!("registry service `{name}`"))
}

impl App {
    pub async fn registry_list(&self, filter: &RegistryFilter) -> Result<Vec<RegistryEntry>> {
        let mut entries = self.store.registry().list().await?;
        entries.retain(|e| {
            filter.category.is_none_or(|c| e.service.category == c)
                && filter
                    .tag
                    .as_ref()
                    .is_none_or(|t| e.service.tags.contains(t))
        });
        Ok(entries)
    }

    pub async fn registry_show(&self, name: &str) -> Result<RegistryEntry> {
        self.store
            .registry()
            .get(name)
            .await?
            .ok_or_else(|| not_found(name))
    }

    pub async fn registry_add(&self, def: ServiceDef) -> Result<RegistryEntry> {
        def.validate()?;
        self.store
            .registry()
            .add_service(&def, Source::Manual)
            .await
            .map_err(|e| match AppError::from(e) {
                AppError::AlreadyExists(_) => {
                    AppError::AlreadyExists(format!("registry service `{}`", def.name))
                }
                e => e,
            })
    }

    pub async fn registry_version_add(
        &self,
        service: &str,
        req: AddVersion,
    ) -> Result<RegistryVersion> {
        let entry = self.registry_show(service).await?;
        let spec = match req.spec {
            Some(spec) => spec,
            None => {
                let source = match &req.copy_from {
                    Some(v) => entry
                        .version(v)
                        .ok_or_else(|| AppError::NotFound(format!("{service}@{v}")))?,
                    None => entry
                        .versions
                        .first()
                        .ok_or_else(|| AppError::Invalid(format!("{service} has no versions")))?,
                };
                source.spec.clone()
            }
        };
        let mut def = VersionDef::new(req.version, req.image, spec);
        def.digest = req.digest;
        def.release_notes = req.release_notes;

        // Validate as part of the full service so spec rules apply.
        let mut check = ServiceDef::from(&entry);
        check.versions = vec![def.clone()];
        check.validate()?;

        self.store
            .registry()
            .add_version(service, &def)
            .await
            .map_err(|e| match AppError::from(e) {
                AppError::AlreadyExists(_) => {
                    AppError::AlreadyExists(format!("{service}@{}", def.version))
                }
                e => e,
            })
    }

    pub async fn registry_deprecate(
        &self,
        service: &str,
        version: &Version,
        deprecated: bool,
    ) -> Result<()> {
        if !self
            .store
            .registry()
            .set_deprecated(service, version, deprecated)
            .await?
        {
            return Err(AppError::NotFound(format!("{service}@{version}")));
        }
        Ok(())
    }

    pub async fn registry_remove(&self, service: &str) -> Result<()> {
        let usage = self.registry_usage(service).await?;
        if !usage.is_empty() {
            return Err(in_use(format!("registry service `{service}`"), &usage));
        }
        if !self.store.registry().delete_service(service).await? {
            return Err(not_found(service));
        }
        Ok(())
    }

    pub async fn registry_version_remove(&self, service: &str, version: &Version) -> Result<()> {
        let usage: Vec<Usage> = self
            .registry_usage(service)
            .await?
            .into_iter()
            .filter(|u| &u.version == version)
            .collect();
        if !usage.is_empty() {
            return Err(in_use(format!("{service}@{version}"), &usage));
        }
        if !self
            .store
            .registry()
            .delete_version(service, version)
            .await?
        {
            return Err(AppError::NotFound(format!("{service}@{version}")));
        }
        Ok(())
    }

    pub async fn registry_import(
        &self,
        file: &RegistryFile,
        mode: ImportMode,
    ) -> Result<ImportReport> {
        let mut names = std::collections::BTreeSet::new();
        for def in &file.services {
            def.validate()?;
            if !names.insert(&def.name) {
                return Err(AppError::Invalid(format!(
                    "registry service `{}` is listed twice",
                    def.name
                )));
            }
        }
        Ok(self
            .store
            .registry()
            .import(&file.services, mode, Source::Import)
            .await?)
    }

    pub async fn registry_export(&self) -> Result<RegistryFile> {
        let entries = self.store.registry().list().await?;
        Ok(RegistryFile {
            services: entries.iter().map(ServiceDef::from).collect(),
        })
    }

    /// Workload services using the registry service `name`.
    pub async fn registry_usage(&self, name: &str) -> Result<Vec<Usage>> {
        Ok(self.store.registry().usage(name).await?)
    }

    /// Services of every workload (or of `workload`) behind the registry's versions, or on
    /// a deprecated one. Run `ikit registry sync` first to learn about new upstream tags.
    pub async fn registry_outdated(&self, workload: Option<&str>) -> Result<Vec<Outdated>> {
        let entries: std::collections::BTreeMap<String, RegistryEntry> = self
            .store
            .registry()
            .list()
            .await?
            .into_iter()
            .map(|e| (e.service.name.clone(), e))
            .collect();
        let workloads = match workload {
            Some(w) => vec![self.workload_get(w).await?],
            None => self.workload_list(None).await?,
        };
        let mut out = Vec::new();
        for wl in workloads {
            for svc in self.services_list(&wl.name).await? {
                let Some(entry) = entries.get(&svc.registry_service) else {
                    continue;
                };
                let newer = |v: Option<&RegistryVersion>| {
                    v.map(|v| v.version.clone()).filter(|v| *v > svc.version)
                };
                let wanted = newer(entry.resolve(&svc.constraint));
                let latest = newer(
                    entry
                        .versions
                        .iter()
                        .filter(|v| !v.deprecated && v.version.pre.is_empty())
                        .max_by(|a, b| a.version.cmp(&b.version)),
                );
                if wanted.is_some() || latest.is_some() || svc.deprecated {
                    out.push(Outdated {
                        workload: wl.name.clone(),
                        service: svc.name,
                        registry_service: svc.registry_service,
                        constraint: svc.constraint,
                        current: svc.version,
                        deprecated: svc.deprecated,
                        wanted,
                        latest,
                    });
                }
            }
        }
        Ok(out)
    }

    /// The registry entry for `name` and the version `constraint` resolves to.
    pub async fn registry_resolve(
        &self,
        name: &str,
        constraint: &VersionConstraint,
    ) -> Result<(RegistryEntry, RegistryVersion)> {
        let entry = self.registry_show(name).await?;
        let version = entry.resolve(constraint).cloned().ok_or_else(|| {
            AppError::Invalid(format!(
                "no version of `{name}` matches `{constraint}` (see `ikit registry show {name}`)"
            ))
        })?;
        Ok((entry, version))
    }
}

fn in_use(what: String, usage: &[Usage]) -> AppError {
    AppError::InUse {
        what,
        by: usage
            .iter()
            .map(|u| format!("{}/{}", u.workload, u.service))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::app;

    #[tokio::test]
    async fn init_seeds_the_registry() {
        let (_dir, app, _) = app().await;
        let data = app
            .registry_list(&RegistryFilter {
                category: Some(Category::Data),
                tag: None,
            })
            .await
            .unwrap();
        let names: Vec<&str> = data.iter().map(|e| e.service.name.as_str()).collect();
        assert_eq!(names, ["mysql", "postgres", "redis"]);
        assert!(data.iter().all(|e| e.service.source == Source::Seed));

        let (_, v) = app
            .registry_resolve("postgres", &"16".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(v.version, Version::new(16, 6, 0));
        assert!(
            app.registry_resolve("postgres", &"^99".parse().unwrap())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn version_add_copies_spec_from_newest() {
        let (_dir, app, _) = app().await;
        let added = app
            .registry_version_add(
                "postgres",
                AddVersion {
                    version: Version::new(17, 3, 0),
                    image: "postgres:17.3".into(),
                    digest: None,
                    release_notes: None,
                    spec: None,
                    copy_from: None,
                },
            )
            .await
            .unwrap();
        let newest_before = app
            .registry_show("postgres")
            .await
            .unwrap()
            .version(&Version::new(17, 2, 0))
            .cloned();
        assert_eq!(Some(added.spec), newest_before.map(|v| v.spec));
    }

    #[tokio::test]
    async fn outdated_reports_wanted_and_latest() {
        let (_dir, app, _) = app().await;
        app.target_attach("laptop", ikit_core::Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        let req = crate::ServiceRequest {
            name: None,
            registry: "postgres".into(),
            constraint: "16".parse().unwrap(),
            group: Category::Data,
            env: [("POSTGRES_PASSWORD".to_owned(), "x".to_owned())].into(),
        };
        app.services_add("wl-acme-dev-001", vec![req])
            .await
            .unwrap();
        let o = &app.registry_outdated(None).await.unwrap()[0];
        assert_eq!(
            (o.current.to_string(), o.wanted.clone(), o.latest.clone()),
            ("16.6.0".to_owned(), None, Some(Version::new(17, 2, 0))),
            "16 allows nothing newer; 17.2 is the newest"
        );

        let add = |v| AddVersion {
            version: v,
            image: "postgres:16.7".into(),
            digest: None,
            release_notes: None,
            spec: None,
            copy_from: Some(Version::new(16, 6, 0)),
        };
        app.registry_version_add("postgres", add(Version::new(16, 7, 0)))
            .await
            .unwrap();
        let o = &app
            .registry_outdated(Some("wl-acme-dev-001"))
            .await
            .unwrap()[0];
        assert_eq!(o.wanted, Some(Version::new(16, 7, 0)));
    }

    #[tokio::test]
    async fn export_import_round_trip() {
        let (_dir, app, _) = app().await;
        let exported = app.registry_export().await.unwrap();
        let report = app
            .registry_import(&exported, ImportMode::Merge)
            .await
            .unwrap();
        assert_eq!(
            report,
            ImportReport::default(),
            "re-importing an export changes nothing"
        );
    }
}
