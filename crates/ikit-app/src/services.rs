use std::collections::{BTreeMap, BTreeSet};

use ikit_core::registry::unknown_env;
use ikit_core::{
    Category, Dependency, PublishedPort, RegistryVersion, ServiceInstance, ServiceSpec,
    VersionConstraint,
};
use ikit_state::{NewService, ServiceUpdate};
use semver::Version;
use serde::Serialize;

use crate::{App, AppError, Result};

/// A registry service to add to a workload.
#[derive(Debug, Clone)]
pub struct ServiceRequest {
    /// Instance name; defaults to the registry service name.
    pub name: Option<String>,
    pub registry: String,
    pub constraint: VersionConstraint,
    pub group: Category,
    /// Env values, secret and non-secret; split by the version's env schema.
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnmetDependency {
    pub service: String,
    pub dependency: Dependency,
}

#[derive(Debug, Clone, Serialize)]
pub struct AddServicesReport {
    pub added: Vec<ServiceInstance>,
    /// Dependencies no service in the workload satisfies. Not an error: the dependency may
    /// live outside the workload (e.g. a managed database).
    pub unmet: Vec<UnmetDependency>,
}

/// Validates `env` against `spec`; returns (non-secret, secret) values.
pub(crate) fn split_env(
    service: &str,
    spec: &ServiceSpec,
    env: BTreeMap<String, String>,
) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let env: BTreeMap<String, String> = env.into_iter().filter(|(_, v)| !v.is_empty()).collect();
    if let Some(unknown) = unknown_env(spec, &env) {
        return Err(AppError::Invalid(format!(
            "`{service}` has no env var(s) {unknown}"
        )));
    }
    for var in &spec.env {
        match env.get(&var.name) {
            None if var.needs_value() => {
                return Err(AppError::Invalid(format!(
                    "`{service}` requires env var {}",
                    var.name
                )));
            }
            Some(v) if !var.allowed.is_empty() && !var.allowed.contains(v) => {
                return Err(AppError::Invalid(format!(
                    "`{service}` env var {} must be one of: {}",
                    var.name,
                    var.allowed.join(", ")
                )));
            }
            _ => {}
        }
    }
    Ok(env
        .into_iter()
        .partition(|(k, _)| !spec.env_var(k).is_some_and(|v| v.secret)))
}

/// Dependencies of `services` not met by any of them.
fn unmet(services: &[(String, String, Version, ServiceSpec)]) -> Vec<UnmetDependency> {
    let installed: Vec<(&str, &Version)> = services
        .iter()
        .map(|(_, r, v, _)| (r.as_str(), v))
        .collect();
    services
        .iter()
        .flat_map(|(name, _, _, spec)| {
            spec.depends_on
                .iter()
                .filter(|d| !d.satisfied_by(installed.iter().copied()))
                .map(|d| UnmetDependency {
                    service: name.clone(),
                    dependency: d.clone(),
                })
        })
        .collect()
}

impl App {
    pub async fn services_list(&self, workload: &str) -> Result<Vec<ServiceInstance>> {
        Ok(self.workload_show(workload).await?.services)
    }

    pub async fn service_show(
        &self,
        workload: &str,
        name: &str,
    ) -> Result<(ServiceInstance, RegistryVersion)> {
        let service = self
            .services_list(workload)
            .await?
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| AppError::NotFound(format!("service `{name}` in {workload}")))?;
        let version = self
            .registry_show(&service.registry_service)
            .await?
            .versions
            .into_iter()
            .find(|v| v.id == service.version_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("{}@{}", service.registry_service, service.version))
            })?;
        Ok((service, version))
    }

    /// Adds services (creating their groups) in one step. Secret env values go to the keychain.
    pub async fn services_add(
        &self,
        workload: &str,
        requests: Vec<ServiceRequest>,
    ) -> Result<AddServicesReport> {
        let wl = self.workload_get(workload).await?;
        let existing = self.store.workloads().services(wl.id).await?;
        let mut taken: BTreeSet<String> = existing.iter().map(|s| s.name.clone()).collect();

        struct Prepared {
            name: String,
            group: Category,
            registry_id: i64,
            constraint: VersionConstraint,
            version: RegistryVersion,
            env: BTreeMap<String, String>,
            secrets: BTreeMap<String, String>,
        }
        let mut prepared = Vec::new();
        for req in requests {
            let (entry, version) = self
                .registry_resolve(&req.registry, &req.constraint)
                .await?;
            let name = req.name.unwrap_or_else(|| req.registry.clone());
            if !ikit_core::is_slug(&name) {
                return Err(AppError::Invalid(format!(
                    "invalid service name `{name}`: use {}",
                    ikit_core::SLUG_RULE
                )));
            }
            if !taken.insert(name.clone()) {
                return Err(AppError::AlreadyExists(format!(
                    "service `{name}` in {workload}"
                )));
            }
            let (env, secrets) = split_env(&name, &version.spec, req.env)?;
            prepared.push(Prepared {
                name,
                group: req.group,
                registry_id: entry.service.id,
                constraint: req.constraint,
                version,
                env,
                secrets,
            });
        }

        let secret_keys: Vec<Vec<String>> = prepared
            .iter()
            .map(|p| p.secrets.keys().cloned().collect())
            .collect();
        let new: Vec<NewService> = prepared
            .iter()
            .zip(&secret_keys)
            .map(|(p, keys)| NewService {
                name: &p.name,
                group: p.group,
                registry_service_id: p.registry_id,
                version_id: p.version.id,
                constraint: &p.constraint,
                env: &p.env,
                secret_env: keys,
            })
            .collect();
        self.store.workloads().add_services(wl.id, &new).await?;

        for p in prepared.iter().filter(|p| !p.secrets.is_empty()) {
            let json = serde_json::to_string(&p.secrets).expect("string map serializes");
            if let Err(e) = self
                .secrets
                .set(&self.service_secret_key(workload, &p.name), &json)
            {
                let names: Vec<String> = prepared.iter().map(|p| p.name.clone()).collect();
                self.store
                    .workloads()
                    .remove_services(wl.id, &names)
                    .await?;
                return Err(e.into());
            }
        }
        self.write_spec(workload).await?;

        let all = self.store.workloads().services(wl.id).await?;
        let added_names: BTreeSet<&str> = prepared.iter().map(|p| p.name.as_str()).collect();
        let specs = self.specs_for(&all).await?;
        let unmet = unmet(&specs)
            .into_iter()
            .filter(|u| added_names.contains(u.service.as_str()))
            .collect();
        let added = all
            .into_iter()
            .filter(|s| added_names.contains(s.name.as_str()))
            .collect();
        Ok(AddServicesReport { added, unmet })
    }

    /// Re-resolves a service's constraint (or `to`) and switches it to the result.
    /// Returns the previous version and the updated service.
    pub async fn service_upgrade(
        &self,
        workload: &str,
        name: &str,
        to: Option<VersionConstraint>,
    ) -> Result<(Version, ServiceInstance)> {
        let (service, _) = self.service_show(workload, name).await?;
        let constraint = to.unwrap_or_else(|| service.constraint.clone());
        let (_, version) = self
            .registry_resolve(&service.registry_service, &constraint)
            .await?;

        // The stored values must still fit the new version's env schema.
        let mut env = service.env.clone();
        for key in &service.secret_env {
            env.insert(key.clone(), "<keychain>".to_owned());
        }
        if let Some(unknown) = unknown_env(&version.spec, &env) {
            return Err(AppError::Invalid(format!(
                "{}@{} has no env var(s) {unknown}; unset them first with `ikit svc set {name} --unset ...`",
                service.registry_service, version.version
            )));
        }
        if let Some(missing) = version
            .spec
            .env
            .iter()
            .find(|v| v.needs_value() && !env.contains_key(&v.name))
        {
            return Err(AppError::Invalid(format!(
                "{}@{} requires env var {}; set it first with `ikit svc set {name} {}=...`",
                service.registry_service, version.version, missing.name, missing.name
            )));
        }

        let wl = self.workload_get(workload).await?;
        let update = ServiceUpdate {
            version_id: version.id,
            constraint: &constraint,
            env: &service.env,
            secret_env: &service.secret_env,
            publish: &service.publish,
        };
        self.store
            .workloads()
            .update_service(wl.id, name, update)
            .await?;
        self.write_spec(workload).await?;
        let (updated, _) = self.service_show(workload, name).await?;
        Ok((service.version, updated))
    }

    /// Sets and unsets env values. Secret values are written to the keychain.
    pub async fn service_set_env(
        &self,
        workload: &str,
        name: &str,
        set: BTreeMap<String, String>,
        unset: &[String],
    ) -> Result<ServiceInstance> {
        let (service, version) = self.service_show(workload, name).await?;
        let spec = &version.spec;
        let key = self.service_secret_key(workload, name);
        let mut secrets: BTreeMap<String, String> = match self.secrets.get(&key)? {
            Some(json) => serde_json::from_str(&json).map_err(|e| {
                AppError::Invalid(format!("keychain entry {key} is not valid JSON: {e}"))
            })?,
            None => BTreeMap::new(),
        };
        let mut env = service.env.clone();
        for k in unset {
            if env.remove(k).is_none() && secrets.remove(k).is_none() {
                return Err(AppError::Invalid(format!(
                    "`{name}` has no value set for {k}"
                )));
            }
        }
        for (k, v) in set {
            if spec.env_var(&k).is_some_and(|e| e.secret) {
                secrets.insert(k, v);
            } else {
                env.insert(k, v);
            }
        }

        // Validate the combined result, then persist.
        let mut combined = env.clone();
        combined.extend(secrets.clone());
        split_env(name, spec, combined)?;

        let secret_env: Vec<String> = secrets.keys().cloned().collect();
        if secrets.is_empty() {
            self.secrets.delete(&key)?;
        } else {
            self.secrets.set(
                &key,
                &serde_json::to_string(&secrets).expect("string map serializes"),
            )?;
        }
        let wl = self.workload_get(workload).await?;
        let update = ServiceUpdate {
            version_id: service.version_id,
            constraint: &service.constraint,
            env: &env,
            secret_env: &secret_env,
            publish: &service.publish,
        };
        self.store
            .workloads()
            .update_service(wl.id, name, update)
            .await?;
        self.write_spec(workload).await?;
        Ok(self.service_show(workload, name).await?.0)
    }

    /// Publishes container ports on the target's host (`add`) and stops publishing host
    /// ports (`remove`). Takes effect on the next apply.
    pub async fn service_publish(
        &self,
        workload: &str,
        name: &str,
        add: Vec<PublishedPort>,
        remove: &[u16],
    ) -> Result<ServiceInstance> {
        let services = self.services_list(workload).await?;
        let service = services
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| AppError::NotFound(format!("service `{name}` in {workload}")))?;
        let mut publish = service.publish.clone();
        for host in remove {
            let before = publish.len();
            publish.retain(|p| p.host != *host);
            if publish.len() == before {
                return Err(AppError::Invalid(format!(
                    "`{name}` does not publish host port {host}"
                )));
            }
        }
        for p in add {
            publish.retain(|q| !(q.host == p.host && q.udp == p.udp));
            publish.push(p);
        }
        // Two services cannot publish the same host port.
        for other in services.iter().filter(|s| s.name != name) {
            if let Some(p) = publish.iter().find(|p| {
                other
                    .publish
                    .iter()
                    .any(|q| q.host == p.host && q.udp == p.udp)
            }) {
                return Err(AppError::Invalid(format!(
                    "host port {} is already published by `{}`",
                    p.host, other.name
                )));
            }
        }
        let wl = self.workload_get(workload).await?;
        let update = ServiceUpdate {
            version_id: service.version_id,
            constraint: &service.constraint,
            env: &service.env,
            secret_env: &service.secret_env,
            publish: &publish,
        };
        self.store
            .workloads()
            .update_service(wl.id, name, update)
            .await?;
        self.write_spec(workload).await?;
        Ok(self.service_show(workload, name).await?.0)
    }

    /// The env a service runs with: registry defaults, values set on the instance, and
    /// secret values from the keychain (`from:` references are not resolved).
    pub(crate) async fn service_env(
        &self,
        workload: &str,
        name: &str,
    ) -> Result<BTreeMap<String, String>> {
        let (service, version) = self.service_show(workload, name).await?;
        let mut env: BTreeMap<String, String> = version
            .spec
            .env
            .iter()
            .filter_map(|v| Some((v.name.clone(), v.default.clone()?)))
            .collect();
        env.extend(service.env.clone());
        if !service.secret_env.is_empty() {
            let key = self.service_secret_key(workload, name);
            if let Some(json) = self.secrets.get(&key)? {
                let secrets: BTreeMap<String, String> =
                    serde_json::from_str(&json).map_err(|e| {
                        AppError::Invalid(format!("keychain entry {key} is not valid JSON: {e}"))
                    })?;
                env.extend(secrets);
            }
        }
        Ok(env)
    }

    /// Removes services and their keychain secrets. Unknown names are an error.
    pub async fn services_remove(&self, workload: &str, names: &[String]) -> Result<()> {
        let wl = self.workload_get(workload).await?;
        let hooks = self.store.hooks().list(wl.id).await?;
        if let Some(h) = hooks.iter().find(|h| {
            h.action
                .service()
                .is_some_and(|s| names.iter().any(|n| n == s))
        }) {
            return Err(AppError::InUse {
                what: format!("service `{}`", h.action.service().unwrap_or_default()),
                by: vec![format!(
                    "hook `{}` (remove it with `ikit hook rm {}`)",
                    h.name, h.name
                )],
            });
        }
        let services = self.store.workloads().services(wl.id).await?;
        for n in names {
            if !services.iter().any(|s| &s.name == n) {
                return Err(AppError::NotFound(format!("service `{n}` in {workload}")));
            }
        }
        self.store.workloads().remove_services(wl.id, names).await?;
        for s in services
            .iter()
            .filter(|s| names.contains(&s.name) && !s.secret_env.is_empty())
        {
            self.secrets
                .delete(&self.service_secret_key(workload, &s.name))?;
        }
        self.write_spec(workload).await?;
        Ok(())
    }

    /// Removes a whole service group. Returns the removed service names.
    pub async fn group_remove(&self, workload: &str, group: Category) -> Result<Vec<String>> {
        let names: Vec<String> = self
            .services_list(workload)
            .await?
            .into_iter()
            .filter(|s| s.group == group)
            .map(|s| s.name)
            .collect();
        if names.is_empty() {
            return Err(AppError::NotFound(format!("{group} group in {workload}")));
        }
        self.services_remove(workload, &names).await?;
        Ok(names)
    }

    /// Unmet dependencies across the whole workload.
    pub async fn workload_unmet_dependencies(
        &self,
        workload: &str,
    ) -> Result<Vec<UnmetDependency>> {
        let services = self.services_list(workload).await?;
        Ok(unmet(&self.specs_for(&services).await?))
    }

    /// `(instance, registry service, version, spec)` for each service.
    async fn specs_for(
        &self,
        services: &[ServiceInstance],
    ) -> Result<Vec<(String, String, Version, ServiceSpec)>> {
        let mut specs: BTreeMap<i64, ServiceSpec> = BTreeMap::new();
        for entry in self.store.registry().list().await? {
            specs.extend(entry.versions.into_iter().map(|v| (v.id, v.spec)));
        }
        Ok(services
            .iter()
            .map(|s| {
                let spec = specs.remove(&s.version_id).unwrap_or_default();
                (
                    s.name.clone(),
                    s.registry_service.clone(),
                    s.version.clone(),
                    spec,
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use ikit_core::Connection;
    use ikit_core::registry::{ServiceDef, VersionDef};

    use super::*;
    use crate::SecretStore;
    use crate::testing::app;

    fn req(
        registry: &str,
        constraint: &str,
        group: Category,
        env: &[(&str, &str)],
    ) -> ServiceRequest {
        ServiceRequest {
            name: None,
            registry: registry.to_owned(),
            constraint: constraint.parse().unwrap(),
            group,
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    async fn setup() -> (
        tempfile::TempDir,
        App,
        std::sync::Arc<crate::MemorySecretStore>,
    ) {
        let (dir, app, secrets) = app().await;
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        let mut api = VersionDef::new(
            Version::new(1, 4, 2),
            "ghcr.io/acme/orders-api:1.4.2".into(),
            ServiceSpec::default(),
        );
        api.depends_on = vec!["postgres@^16".parse().unwrap()];
        app.registry_add(ServiceDef {
            name: "orders-api".into(),
            display_name: None,
            description: None,
            category: Category::App,
            tags: vec![],
            owner: None,
            homepage: None,
            versions: vec![api],
        })
        .await
        .unwrap();
        (dir, app, secrets)
    }

    #[tokio::test]
    async fn add_splits_secrets_and_reports_unmet_dependencies() {
        let (_dir, app, secrets) = setup().await;

        let report = app
            .services_add(
                "wl-acme-dev-001",
                vec![req("orders-api", "^1", Category::App, &[])],
            )
            .await
            .unwrap();
        assert_eq!(report.added.len(), 1);
        assert_eq!(report.unmet.len(), 1);
        assert_eq!(report.unmet[0].dependency.to_string(), "postgres@^16");

        let pg = req(
            "postgres",
            "16",
            Category::Data,
            &[("POSTGRES_PASSWORD", "pw"), ("POSTGRES_DB", "orders")],
        );
        let report = app.services_add("wl-acme-dev-001", vec![pg]).await.unwrap();
        let svc = &report.added[0];
        assert_eq!(svc.version, Version::new(16, 6, 0));
        assert_eq!(
            svc.env,
            BTreeMap::from([("POSTGRES_DB".to_owned(), "orders".to_owned())])
        );
        assert_eq!(svc.secret_env, ["POSTGRES_PASSWORD"]);
        assert_eq!(
            secrets
                .get("org/acme/workload/wl-acme-dev-001/postgres")
                .unwrap()
                .as_deref(),
            Some(r#"{"POSTGRES_PASSWORD":"pw"}"#)
        );
        assert!(
            app.workload_unmet_dependencies("wl-acme-dev-001")
                .await
                .unwrap()
                .is_empty()
        );

        let spec = std::fs::read_to_string(app.org().workload_spec("wl-acme-dev-001")).unwrap();
        assert!(
            spec.contains("resolved: 16.6.0") && !spec.contains("pw"),
            "{spec}"
        );
    }

    #[tokio::test]
    async fn add_validates_env_and_names() {
        let (_dir, app, _) = setup().await;
        let missing = app
            .services_add(
                "wl-acme-dev-001",
                vec![req("postgres", "16", Category::Data, &[])],
            )
            .await;
        assert!(matches!(missing, Err(AppError::Invalid(m)) if m.contains("POSTGRES_PASSWORD")));
        let unknown = app
            .services_add(
                "wl-acme-dev-001",
                vec![req(
                    "postgres",
                    "16",
                    Category::Data,
                    &[("POSTGRES_PASSWORD", "x"), ("NOPE", "1")],
                )],
            )
            .await;
        assert!(matches!(unknown, Err(AppError::Invalid(m)) if m.contains("NOPE")));

        let pg = req(
            "postgres",
            "16",
            Category::Data,
            &[("POSTGRES_PASSWORD", "x")],
        );
        let dup = app
            .services_add("wl-acme-dev-001", vec![pg.clone(), pg])
            .await;
        assert!(matches!(dup, Err(AppError::AlreadyExists(_))));
        assert!(
            app.services_list("wl-acme-dev-001")
                .await
                .unwrap()
                .is_empty(),
            "nothing written on failure"
        );
    }

    #[tokio::test]
    async fn upgrade_set_and_remove() {
        let (_dir, app, secrets) = setup().await;
        let pg = req(
            "postgres",
            "16",
            Category::Data,
            &[("POSTGRES_PASSWORD", "pw")],
        );
        app.services_add("wl-acme-dev-001", vec![pg]).await.unwrap();

        let (old, new) = app
            .service_upgrade("wl-acme-dev-001", "postgres", Some("^17".parse().unwrap()))
            .await
            .unwrap();
        assert_eq!(old, Version::new(16, 6, 0));
        assert_eq!(new.version, Version::new(17, 2, 0));
        assert_eq!(new.constraint.to_string(), "^17");

        let set = BTreeMap::from([
            ("POSTGRES_DB".to_owned(), "x".to_owned()),
            ("POSTGRES_PASSWORD".to_owned(), "new".to_owned()),
        ]);
        let svc = app
            .service_set_env("wl-acme-dev-001", "postgres", set, &[])
            .await
            .unwrap();
        assert_eq!(svc.env["POSTGRES_DB"], "x");
        assert_eq!(
            secrets
                .get("org/acme/workload/wl-acme-dev-001/postgres")
                .unwrap()
                .as_deref(),
            Some(r#"{"POSTGRES_PASSWORD":"new"}"#)
        );
        // The password is required, so it cannot be unset.
        assert!(
            app.service_set_env(
                "wl-acme-dev-001",
                "postgres",
                BTreeMap::new(),
                &["POSTGRES_PASSWORD".into()]
            )
            .await
            .is_err()
        );

        assert_eq!(
            app.group_remove("wl-acme-dev-001", Category::Data)
                .await
                .unwrap(),
            ["postgres"]
        );
        assert_eq!(
            secrets
                .get("org/acme/workload/wl-acme-dev-001/postgres")
                .unwrap(),
            None
        );
        assert!(
            app.registry_remove("postgres").await.is_ok(),
            "no longer in use"
        );
    }

    #[tokio::test]
    async fn registry_entries_in_use_cannot_be_removed() {
        let (_dir, app, _) = setup().await;
        app.services_add(
            "wl-acme-dev-001",
            vec![req("redis", "latest", Category::Data, &[])],
        )
        .await
        .unwrap();
        match app.registry_remove("redis").await {
            Err(AppError::InUse { by, .. }) => assert_eq!(by, ["wl-acme-dev-001/redis"]),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            app.registry_version_remove("redis", &Version::new(7, 4, 1))
                .await,
            Err(AppError::InUse { .. })
        ));
    }
}
