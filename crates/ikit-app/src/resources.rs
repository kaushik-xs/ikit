//! Resources services need besides their containers: the org's backends and their per-env
//! bindings, the buckets and queues recorded for each workload, and creating, adopting and
//! deleting them through a [`ResourceDriver`].
//!
//! A service's own resources (`needs` with an `api`) get a row in the state database when the
//! service is added, as `pending`. `ikit apply` creates them on the backend bound to the
//! workload's env and records what they turned out to be. Resources shared with `use` have no
//! row of their own: they resolve to the owning service's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use ikit_core::resource::default_external_id;
use ikit_core::{
    Backend, BackendOptions, Binding, Need, NeedOptions, ProfileKind, Resource, ResourceApi,
    ResourceOrigin, ResourceProvider, ResourceStatus, ServiceSpec, Workload,
};
use ikit_provider::compose::ResourceValues;
use ikit_state::{NewBackend, NewResource, ResourceUpdate};
use serde::Serialize;

use crate::{App, AppError, Result};

mod aws;
pub(crate) mod iam;
mod identities;

pub use identities::{IdentityAction, IdentityPlan, IdentityPolicies};

/// Shown in place of a value that exists only once `ikit apply` has created the resource.
pub const PENDING_VALUE: &str = "<created by ikit apply>";
/// Shown in place of values of a resource whose backend isn't known yet.
pub const UNBOUND_VALUE: &str = "<no backend: see ikit res bind>";

/// Changes to a backend. `None` leaves a setting alone; `boundary: Some(None)` clears it.
#[derive(Debug, Clone, Default)]
pub struct BackendChanges {
    pub per_service_keys: Option<bool>,
    pub boundary: Option<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct NewBackendRequest {
    pub name: String,
    pub api: ResourceApi,
    pub provider: ResourceProvider,
    pub profile: Option<String>,
    pub options: BackendOptions,
}

/// A recorded resource with what `ikit res list` shows next to it.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceInfo {
    #[serde(flatten)]
    pub resource: Resource,
    /// The env's binding, for a resource with no backend of its own yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_to: Option<String>,
    /// Services sharing it with `use`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub used_by: Vec<String>,
}

impl ResourceInfo {
    /// The backend the resource is on, or will be created on.
    pub fn backend(&self) -> Option<&str> {
        self.resource
            .backend
            .as_deref()
            .or(self.bound_to.as_deref())
    }
}

/// A resource the next apply creates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourcePlan {
    /// `<service>.<resource>`.
    pub resource: String,
    pub api: ResourceApi,
    pub external_id: String,
    /// `None` when no backend is bound, which blocks the apply.
    pub backend: Option<String>,
}

/// What a driver is asked to create.
#[derive(Debug, Clone)]
pub(crate) struct Want<'a> {
    pub api: ResourceApi,
    pub name: &'a str,
    pub options: &'a NeedOptions,
    pub tags: &'a BTreeMap<String, String>,
}

/// Creates, finds and deletes resources at one backend. Outputs are the non-secret fields
/// only known once a resource exists (`url`, `arn`, `dlq_url`).
#[async_trait]
pub(crate) trait ResourceDriver: Send + Sync {
    /// Creates the resource, or brings an existing one with that name in line. Safe to repeat.
    async fn ensure(&self, want: &Want<'_>) -> Result<BTreeMap<String, String>>;
    /// Finds an existing resource by name (or URL, for queues): its name and outputs.
    async fn inspect(
        &self,
        api: ResourceApi,
        id: &str,
    ) -> Result<Option<(String, BTreeMap<String, String>)>>;
    async fn delete(
        &self,
        api: ResourceApi,
        name: &str,
        outputs: &BTreeMap<String, String>,
    ) -> Result<()>;

    // IAM, for per-service keys. Drivers without it refuse.

    /// The provisioning identity: its account ID and ARN.
    async fn whoami(&self) -> Result<(String, String)> {
        Err(no_iam())
    }
    /// Creates the user (or checks the existing one carries `boundary`); returns its ARN.
    async fn ensure_user(&self, _user: &IamUser<'_>) -> Result<String> {
        Err(no_iam())
    }
    async fn put_user_policy(&self, _user: &str, _name: &str, _document: &str) -> Result<()> {
        Err(no_iam())
    }
    /// IDs of the user's access keys.
    async fn access_keys(&self, _user: &str) -> Result<Vec<String>> {
        Err(no_iam())
    }
    async fn create_access_key(&self, _user: &str) -> Result<Keys> {
        Err(no_iam())
    }
    async fn delete_access_key(&self, _user: &str, _id: &str) -> Result<()> {
        Err(no_iam())
    }
    /// Deletes the user with its keys and inline policies; nothing if it's gone already.
    async fn delete_user(&self, _user: &str) -> Result<()> {
        Err(no_iam())
    }
    /// Waits until a new key is accepted (IAM is eventually consistent).
    async fn wait_for_key(&self, _keys: &Keys) -> Result<()> {
        Err(no_iam())
    }
}

fn no_iam() -> AppError {
    AppError::Invalid(
        "this backend can't create IAM users; per-service keys need AWS itself".to_owned(),
    )
}

/// An IAM user to create for a service.
#[derive(Debug, Clone)]
pub(crate) struct IamUser<'a> {
    pub name: &'a str,
    pub path: &'a str,
    pub boundary: &'a str,
    pub tags: &'a BTreeMap<String, String>,
}

/// How to reach a backend: region, endpoint and the credentials services get too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Access {
    pub region: String,
    pub endpoint: Option<String>,
    /// `None`: the AWS SDK's default chain (environment, ~/.aws default, instance role).
    pub keys: Option<Keys>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Keys {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// `<service>.<resource>` → its parts.
pub fn parse_address(address: &str) -> Result<(&str, &str)> {
    address
        .split_once('.')
        .filter(|(s, r)| !s.is_empty() && !r.is_empty())
        .ok_or_else(|| {
            AppError::Invalid(format!(
                "`{address}` is not a resource; use <service>.<resource>, e.g. simulation-service.jobs"
            ))
        })
}

/// Fields of a resource that need no backend call: S3 names its buckets globally.
fn known_outputs(api: ResourceApi, name: &str) -> BTreeMap<String, String> {
    match api {
        ResourceApi::S3 => BTreeMap::from([
            ("url".to_owned(), format!("s3://{name}")),
            ("arn".to_owned(), format!("arn:aws:s3:::{name}")),
        ]),
        ResourceApi::Sqs => BTreeMap::new(),
    }
}

/// A lowercase, digits-only suffix for bucket names, which are global.
fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("system randomness");
    bytes
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect()
}

/// The owner of a `use` need: an instance name, or a registry service with exactly one
/// instance in the workload.
fn owner<'a>(
    specs: &'a [(String, String, semver::Version, ServiceSpec)],
    service: &str,
) -> Option<&'a str> {
    if let Some((name, ..)) = specs.iter().find(|(n, ..)| n == service) {
        return Some(name);
    }
    let mut by_registry = specs.iter().filter(|(_, r, ..)| r == service);
    match (by_registry.next(), by_registry.next()) {
        (Some((name, ..)), None) => Some(name),
        _ => None,
    }
}

impl App {
    // ---- backends and bindings ----

    pub async fn resource_backend_add(&self, req: NewBackendRequest) -> Result<Backend> {
        if !ikit_core::is_slug(&req.name) {
            return Err(AppError::Invalid(format!(
                "invalid backend name `{}`: use {}",
                req.name,
                ikit_core::SLUG_RULE
            )));
        }
        if let Some(endpoint) = &req.options.endpoint
            && !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
        {
            return Err(AppError::Invalid(format!(
                "endpoint `{endpoint}` must start with http:// or https://"
            )));
        }
        if req.options.per_service_keys {
            identities::check_per_service(
                &req.name,
                req.options.endpoint.as_deref(),
                req.options.boundary.as_deref(),
            )?;
        }
        if let Some(profile) = &req.profile {
            self.profile_show(ProfileKind::Aws, profile).await?;
        }
        let new = NewBackend {
            name: &req.name,
            api: req.api,
            provider: req.provider,
            profile: req.profile.as_deref(),
            options: &req.options,
        };
        self.store
            .resources()
            .insert_backend(new)
            .await
            .map_err(|e| match AppError::from(e) {
                AppError::AlreadyExists(_) => {
                    AppError::AlreadyExists(format!("backend `{}`", req.name))
                }
                e => e,
            })
    }

    /// Turns per-service keys on or off, or changes the boundary. Takes effect on the next apply
    /// of each workload using the backend.
    pub async fn resource_backend_set(
        &self,
        name: &str,
        changes: BackendChanges,
    ) -> Result<Backend> {
        let mut b = self.resource_backend_show(name).await?;
        if let Some(on) = changes.per_service_keys {
            b.options.per_service_keys = on;
        }
        if let Some(boundary) = changes.boundary {
            b.options.boundary = boundary;
        }
        if b.options.per_service_keys {
            identities::check_per_service(
                name,
                b.options.endpoint.as_deref(),
                b.options.boundary.as_deref(),
            )?;
        }
        self.store
            .resources()
            .set_backend_options(name, &b.options)
            .await?;
        Ok(b)
    }

    pub async fn resource_backend_list(&self) -> Result<Vec<Backend>> {
        Ok(self.store.resources().backends().await?)
    }

    pub async fn resource_backend_show(&self, name: &str) -> Result<Backend> {
        self.store
            .resources()
            .backend(name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("backend `{name}`")))
    }

    /// Refused while an env is bound to it or a resource is on it.
    pub async fn resource_backend_remove(&self, name: &str) -> Result<()> {
        self.resource_backend_show(name).await?;
        let mut by: Vec<String> = self
            .resource_bindings()
            .await?
            .into_iter()
            .filter(|b| b.backend == name)
            .map(|b| {
                format!(
                    "env {} (unbind with `ikit res backend unbind {} {}`)",
                    b.env, b.env, b.api
                )
            })
            .collect();
        by.extend(
            self.store
                .resources()
                .list(None)
                .await?
                .into_iter()
                .filter(|r| r.backend.as_deref() == Some(name))
                .map(|r| format!("{} in {}", r.address(), r.workload)),
        );
        if !by.is_empty() {
            return Err(AppError::InUse {
                what: format!("backend `{name}`"),
                by,
            });
        }
        self.store.resources().delete_backend(name).await?;
        Ok(())
    }

    pub async fn resource_bindings(&self) -> Result<Vec<Binding>> {
        Ok(self.store.resources().bindings().await?)
    }

    /// Makes `backend` provide its API for workloads in `env` from now on. Resources that
    /// already exist stay where they are.
    pub async fn resource_bind_env(&self, env: &str, backend: &str) -> Result<Binding> {
        if !self.config()?.envs.iter().any(|e| e == env) {
            return Err(AppError::Invalid(format!(
                "unknown env `{env}` (this org's envs: {})",
                self.config()?.envs.join(", ")
            )));
        }
        self.resource_backend_show(backend).await?;
        Ok(self.store.resources().bind(env, backend).await?)
    }

    pub async fn resource_unbind_env(&self, env: &str, api: ResourceApi) -> Result<()> {
        if !self.store.resources().unbind(env, api).await? {
            return Err(AppError::NotFound(format!(
                "binding for {api} in env {env}"
            )));
        }
        Ok(())
    }

    /// The backend bound to `api` for `env`, if any.
    async fn bound_backend(&self, env: &str, api: ResourceApi) -> Result<Option<String>> {
        Ok(self
            .resource_bindings()
            .await?
            .into_iter()
            .find(|b| b.env == env && b.api == api)
            .map(|b| b.backend))
    }

    // ---- workload resources ----

    /// Brings a workload's resource rows in line with its services' needs: new ones are
    /// recorded as pending, ones no longer needed are dropped if they were never created and
    /// marked orphaned otherwise.
    pub(crate) async fn sync_resources(&self, wl: &Workload) -> Result<()> {
        let services = self.store.workloads().services(wl.id).await?;
        let specs = self.specs_for(&services).await?;
        let repo = self.store.resources();
        let rows = repo.list(Some(wl.id)).await?;
        let mut wanted = BTreeSet::new();
        for (service, _, _, spec) in &specs {
            for need in &spec.needs {
                let Some(api) = need.api else { continue };
                wanted.insert((service.clone(), need.name.clone()));
                let row = rows
                    .iter()
                    .find(|r| &r.service == service && r.name == need.name);
                match row {
                    None => {
                        let external_id = default_external_id(
                            api,
                            &wl.name,
                            service,
                            &need.name,
                            &random_suffix(),
                        );
                        repo.insert(
                            wl.id,
                            NewResource {
                                service,
                                name: &need.name,
                                api,
                                backend: None,
                                external_id: &external_id,
                                outputs: &BTreeMap::new(),
                                origin: ResourceOrigin::Created,
                                status: ResourceStatus::Pending,
                            },
                        )
                        .await?;
                    }
                    Some(r) if r.api != api => {
                        return Err(AppError::Invalid(format!(
                            "resource {} is a {} resource, but `{service}` now needs a {api} one; \
                             remove the old one with `ikit res rm {}` after removing the service",
                            r.address(),
                            r.api,
                            r.address()
                        )));
                    }
                    Some(r) if r.orphaned => {
                        let mut back = r.clone();
                        back.orphaned = false;
                        repo.update(r.id, ResourceUpdate::from(&back)).await?;
                    }
                    Some(_) => {}
                }
            }
        }
        for r in rows
            .iter()
            .filter(|r| !wanted.contains(&(r.service.clone(), r.name.clone())))
        {
            if r.status == ResourceStatus::Pending {
                repo.delete(r.id).await?;
            } else if !r.orphaned {
                let mut orphan = r.clone();
                orphan.orphaned = true;
                repo.update(r.id, ResourceUpdate::from(&orphan)).await?;
            }
        }
        Ok(())
    }

    /// Resources of one workload, or of every workload in the org.
    pub async fn resources_list(&self, workload: Option<&str>) -> Result<Vec<ResourceInfo>> {
        let workloads = match workload {
            Some(w) => vec![self.workload_get(w).await?],
            None => self.workload_list(None).await?,
        };
        let bindings = self.resource_bindings().await?;
        let mut out = Vec::new();
        for wl in workloads {
            let services = self.store.workloads().services(wl.id).await?;
            let specs = self.specs_for(&services).await?;
            for r in self.store.resources().list(Some(wl.id)).await? {
                let used_by = specs
                    .iter()
                    .flat_map(|(name, _, _, spec)| {
                        spec.needs
                            .iter()
                            .filter_map(Need::used)
                            .filter(|(s, res)| {
                                owner(&specs, s) == Some(r.service.as_str()) && *res == r.name
                            })
                            .map(move |_| name.clone())
                    })
                    .collect();
                let bound_to = match &r.backend {
                    Some(_) => None,
                    None => bindings
                        .iter()
                        .find(|b| b.env == wl.env && b.api == r.api)
                        .map(|b| b.backend.clone()),
                };
                out.push(ResourceInfo {
                    resource: r,
                    bound_to,
                    used_by,
                });
            }
        }
        Ok(out)
    }

    pub async fn resource_show(&self, workload: &str, address: &str) -> Result<ResourceInfo> {
        let (service, name) = parse_address(address)?;
        self.resources_list(Some(workload))
            .await?
            .into_iter()
            .find(|r| r.resource.service == service && r.resource.name == name)
            .ok_or_else(|| AppError::NotFound(format!("resource `{address}` in {workload}")))
    }

    /// Puts a resource that doesn't exist yet on `backend`, overriding the env's binding.
    pub async fn resource_bind(
        &self,
        workload: &str,
        address: &str,
        backend: &str,
    ) -> Result<Resource> {
        let info = self.resource_show(workload, address).await?;
        let mut r = info.resource;
        if r.status != ResourceStatus::Pending {
            return Err(AppError::Invalid(format!(
                "{address} already exists on {}; it stays there",
                r.backend.as_deref().unwrap_or("its backend")
            )));
        }
        let b = self.resource_backend_show(backend).await?;
        if b.api != r.api {
            return Err(AppError::Invalid(format!(
                "backend `{backend}` provides {}, but {address} needs {}",
                b.api, r.api
            )));
        }
        r.backend = Some(b.name);
        self.store
            .resources()
            .update(r.id, ResourceUpdate::from(&r))
            .await?;
        self.write_spec(workload).await?;
        Ok(r)
    }

    /// Records an existing bucket or queue as the resource, instead of creating one. `id` is
    /// its name, or a queue's URL. ikit never deletes adopted resources, only forgets them.
    pub async fn resource_adopt(
        &self,
        workload: &str,
        address: &str,
        id: &str,
    ) -> Result<Resource> {
        let wl = self.workload_get(workload).await?;
        let info = self.resource_show(workload, address).await?;
        let mut r = info.resource.clone();
        if r.status == ResourceStatus::Ready {
            return Err(AppError::Invalid(format!(
                "{address} already exists ({}); remove it with `ikit res rm {address}` first",
                r.external_id
            )));
        }
        let backend = match info.backend() {
            Some(b) => self.resource_backend_show(b).await?,
            None => return Err(self.unbound(&wl, &r)),
        };
        let driver = self.resource_driver(&backend).await?;
        let (name, outputs) = driver.inspect(r.api, id).await?.ok_or_else(|| {
            AppError::NotFound(format!("{} `{id}` on backend {}", r.kind(), backend.name))
        })?;
        r.external_id = name;
        r.outputs = known_outputs(r.api, &r.external_id);
        r.outputs.extend(outputs);
        // Recorded, so rendering needs no credentials to know it.
        r.outputs.insert(
            "region".to_owned(),
            self.backend_access(&backend).await?.region,
        );
        r.backend = Some(backend.name);
        r.origin = ResourceOrigin::Adopted;
        r.status = ResourceStatus::Ready;
        self.store
            .resources()
            .update(r.id, ResourceUpdate::from(&r))
            .await?;
        self.write_spec(workload).await?;
        Ok(r)
    }

    /// Removes a resource no service needs any more. One ikit created is deleted at its
    /// backend (a bucket must be empty) unless `keep`; an adopted one is only forgotten.
    /// Returns whether it was deleted at the backend.
    pub async fn resource_remove(&self, workload: &str, address: &str, keep: bool) -> Result<bool> {
        let r = self.resource_show(workload, address).await?.resource;
        if !r.orphaned {
            return Err(AppError::InUse {
                what: format!("resource {address}"),
                by: vec![format!(
                    "service `{}` (remove it first with `ikit svc rm {}`)",
                    r.service, r.service
                )],
            });
        }
        let deleted = self.forget_resource(&r, keep).await?;
        // The service's last resource gone: its IAM user has nothing left to reach.
        let wl = self.workload_get(workload).await?;
        self.forget_service_identities(&wl, &r.service, keep)
            .await?;
        self.write_spec(workload).await?;
        Ok(deleted)
    }

    /// Deletes `r` at its backend if ikit created it and not `keep`, then forgets it.
    async fn forget_resource(&self, r: &Resource, keep: bool) -> Result<bool> {
        let delete =
            !keep && r.origin == ResourceOrigin::Created && r.status == ResourceStatus::Ready;
        if delete {
            let backend = self
                .resource_backend_show(r.backend.as_deref().unwrap_or_default())
                .await?;
            self.resource_driver(&backend)
                .await?
                .delete(r.api, &r.external_id, &r.outputs)
                .await?;
        }
        self.store.resources().delete(r.id).await?;
        Ok(delete)
    }

    /// Resources of `wl` that ikit created and that still exist, which `wl rm` would leave
    /// behind.
    pub(crate) async fn created_resources(&self, wl: &Workload) -> Result<Vec<Resource>> {
        Ok(self
            .store
            .resources()
            .list(Some(wl.id))
            .await?
            .into_iter()
            .filter(|r| r.origin == ResourceOrigin::Created && r.status == ResourceStatus::Ready)
            .collect())
    }

    /// Deletes (or, with `keep`, just forgets) every resource of `wl`.
    pub(crate) async fn forget_resources(&self, wl: &Workload, keep: bool) -> Result<()> {
        for r in self.store.resources().list(Some(wl.id)).await? {
            self.forget_resource(&r, keep).await?;
        }
        Ok(())
    }

    fn unbound(&self, wl: &Workload, r: &Resource) -> AppError {
        AppError::Invalid(format!(
            "{} needs a {} backend: bind one for env {} with `ikit res backend bind {} <backend>`, \
             or for this resource with `ikit res bind {} <backend> --wl {}`",
            r.address(),
            r.api,
            wl.env,
            wl.env,
            r.address(),
            wl.name
        ))
    }

    /// Resources the next apply creates, and why it can't if a backend is missing.
    pub(crate) async fn resource_plans(
        &self,
        wl: &Workload,
    ) -> Result<(Vec<ResourcePlan>, Vec<String>)> {
        let mut plans = Vec::new();
        let mut blockers = Vec::new();
        for r in self.store.resources().list(Some(wl.id)).await? {
            if r.status != ResourceStatus::Pending || r.orphaned {
                continue;
            }
            let backend = match &r.backend {
                Some(b) => Some(b.clone()),
                None => self.bound_backend(&wl.env, r.api).await?,
            };
            if backend.is_none() {
                blockers.push(self.unbound(wl, &r).to_string());
            }
            plans.push(ResourcePlan {
                resource: r.address(),
                api: r.api,
                external_id: r.external_id.clone(),
                backend,
            });
        }
        Ok((plans, blockers))
    }

    /// Creates the workload's pending resources. Each is recorded as soon as it exists, so a
    /// failure part-way leaves the rest pending for the next apply. Returns the addresses
    /// created.
    pub(crate) async fn ensure_resources(&self, wl: &Workload) -> Result<Vec<String>> {
        let services = self.store.workloads().services(wl.id).await?;
        let specs = self.specs_for(&services).await?;
        let mut created = Vec::new();
        for mut r in self.store.resources().list(Some(wl.id)).await? {
            if r.status != ResourceStatus::Pending || r.orphaned {
                continue;
            }
            let backend = match &r.backend {
                Some(b) => b.clone(),
                None => self
                    .bound_backend(&wl.env, r.api)
                    .await?
                    .ok_or_else(|| self.unbound(wl, &r))?,
            };
            let backend = self.resource_backend_show(&backend).await?;
            let options = specs
                .iter()
                .find(|(n, ..)| *n == r.service)
                .and_then(|(.., spec)| spec.needs.iter().find(|n| n.name == r.name))
                .map(|n| n.options.clone())
                .unwrap_or_default();
            let tags = BTreeMap::from([
                ("ikit:org".to_owned(), self.org.name.clone()),
                ("ikit:workload".to_owned(), wl.name.clone()),
                ("ikit:service".to_owned(), r.service.clone()),
                ("ikit:resource".to_owned(), r.name.clone()),
            ]);
            let want = Want {
                api: r.api,
                name: &r.external_id,
                options: &options,
                tags: &tags,
            };
            let outputs = self.resource_driver(&backend).await?.ensure(&want).await?;
            r.outputs = known_outputs(r.api, &r.external_id);
            r.outputs.extend(outputs);
            r.outputs.insert(
                "region".to_owned(),
                self.backend_access(&backend).await?.region,
            );
            r.backend = Some(backend.name);
            r.status = ResourceStatus::Ready;
            self.store
                .resources()
                .update(r.id, ResourceUpdate::from(&r))
                .await?;
            created.push(r.address());
        }
        Ok(created)
    }

    /// What each service can read from its resources, by service and resource name. Values
    /// only known after creation are [`PENDING_VALUE`] until then.
    pub(crate) async fn resource_inputs(
        &self,
        wl: &Workload,
    ) -> Result<BTreeMap<String, BTreeMap<String, ResourceValues>>> {
        let rows = self.store.resources().list(Some(wl.id)).await?;
        if rows.is_empty() {
            return Ok(BTreeMap::new());
        }
        let services = self.store.workloads().services(wl.id).await?;
        let specs = self.specs_for(&services).await?;
        let mut access: BTreeMap<String, Access> = BTreeMap::new();
        let mut own: BTreeMap<(String, String), ResourceValues> = BTreeMap::new();
        // Resources on backends with per-service keys, by owner: the provisioning profile.
        let mut per_service: BTreeMap<(String, String), String> = BTreeMap::new();
        for r in rows.iter().filter(|r| !r.orphaned) {
            let backend = match &r.backend {
                Some(b) => Some(b.clone()),
                None => self.bound_backend(&wl.env, r.api).await?,
            };
            let mut fields = BTreeMap::from([("name".to_owned(), r.external_id.clone())]);
            match backend {
                Some(b) => {
                    let backend = self.resource_backend_show(&b).await?;
                    if !access.contains_key(&b) {
                        // Services with keys of their own don't need the provisioning ones, so
                        // a machine without them can still deploy.
                        let a = if backend.options.per_service_keys {
                            Access {
                                region: self.backend_region(&backend).await?,
                                endpoint: backend.options.endpoint.clone(),
                                keys: None,
                            }
                        } else {
                            self.backend_access(&backend).await?
                        };
                        access.insert(b.clone(), a);
                    }
                    if backend.options.per_service_keys {
                        per_service.insert(
                            (r.service.clone(), r.name.clone()),
                            backend.profile.clone().unwrap_or_default(),
                        );
                    }
                    let a = &access[&b];
                    fields.insert("region".to_owned(), a.region.clone());
                    if let Some(e) = &a.endpoint {
                        fields.insert("endpoint".to_owned(), e.clone());
                    }
                    if let Some(k) = &a.keys {
                        fields.insert("access_key_id".to_owned(), k.access_key_id.clone());
                        fields.insert("secret_access_key".to_owned(), k.secret_access_key.clone());
                    }
                    fields.extend(known_outputs(r.api, &r.external_id));
                    if r.status == ResourceStatus::Ready {
                        fields.extend(r.outputs.clone());
                    } else if r.api == ResourceApi::Sqs {
                        for f in ["url", "arn", "dlq_url"] {
                            fields.insert(f.to_owned(), PENDING_VALUE.to_owned());
                        }
                    }
                }
                None => {
                    for (f, _) in r.api.fields().iter().filter(|(f, _)| *f != "name") {
                        fields.insert((*f).to_owned(), UNBOUND_VALUE.to_owned());
                    }
                }
            }
            own.insert(
                (r.service.clone(), r.name.clone()),
                ResourceValues { api: r.api, fields },
            );
        }
        let mut out: BTreeMap<String, BTreeMap<String, ResourceValues>> = BTreeMap::new();
        for (service, _, _, spec) in &specs {
            let mut mine = BTreeMap::new();
            for need in &spec.needs {
                let key = match need.used() {
                    None => (service.clone(), need.name.clone()),
                    Some((s, res)) => {
                        let o = owner(&specs, s).ok_or_else(|| {
                            AppError::Invalid(format!(
                                "`{service}` uses resource {s}.{res}, but {} has no service `{s}`; add it, or remove `{service}`",
                                wl.name
                            ))
                        })?;
                        (o.to_owned(), res.to_owned())
                    }
                };
                match own.get(&key) {
                    Some(v) => {
                        let mut v = v.clone();
                        // This service's own key, not the backend's.
                        if let Some(profile) = per_service.get(&key) {
                            let keys = self.identity_key_for(wl, service, profile).await?;
                            let (id, secret) = match keys {
                                Some(k) => (k.access_key_id, k.secret_access_key),
                                None => (PENDING_VALUE.to_owned(), PENDING_VALUE.to_owned()),
                            };
                            v.fields.insert("access_key_id".to_owned(), id);
                            v.fields.insert("secret_access_key".to_owned(), secret);
                        }
                        mine.insert(need.name.clone(), v);
                    }
                    None if need.uses.is_some() => {
                        return Err(AppError::Invalid(format!(
                            "`{service}` uses resource {}.{}, which `{}` doesn't declare",
                            key.0, key.1, key.0
                        )));
                    }
                    // Not recorded yet; rendering reports env vars that read it.
                    None => {}
                }
            }
            out.insert(service.clone(), mine);
        }
        Ok(out)
    }

    /// A backend's region without its credentials: its own setting, else its profile's, else
    /// the named `~/.aws` profile's if this machine has it, else us-east-1.
    pub(crate) async fn backend_region(&self, backend: &Backend) -> Result<String> {
        if let Some(r) = &backend.options.region {
            return Ok(r.clone());
        }
        let values = match &backend.profile {
            Some(p) => self.profile_values(ProfileKind::Aws, p).await?,
            None => BTreeMap::new(),
        };
        let get = |k: &str| values.get(k).filter(|v| !v.is_empty()).cloned();
        Ok(get("region")
            .or_else(|| {
                get("aws_profile")
                    .and_then(|n| crate::aws_shared::load(&n).ok())
                    .and_then(|s| s.region)
            })
            .unwrap_or_else(|| "us-east-1".to_owned()))
    }

    /// Region, endpoint and credentials of a backend, from its options and aws profile.
    pub(crate) async fn backend_access(&self, backend: &Backend) -> Result<Access> {
        let values = match &backend.profile {
            Some(p) => self.profile_values(ProfileKind::Aws, p).await?,
            None => BTreeMap::new(),
        };
        let get = |k: &str| values.get(k).filter(|v| !v.is_empty()).cloned();
        // Static keys on the ikit profile win over a named profile in ~/.aws.
        let shared = match (get("access_key_id"), get("aws_profile")) {
            (None, Some(name)) => Some(crate::aws_shared::load(&name)?),
            _ => None,
        };
        let region = backend
            .options
            .region
            .clone()
            .or_else(|| get("region"))
            .or_else(|| shared.as_ref().and_then(|s| s.region.clone()))
            .unwrap_or_else(|| "us-east-1".to_owned());
        let keys = match (get("access_key_id"), get("secret_access_key")) {
            (Some(id), Some(secret)) => Some(Keys {
                access_key_id: id,
                secret_access_key: secret,
                session_token: get("session_token"),
            }),
            _ => shared.map(|s| Keys {
                access_key_id: s.access_key_id,
                secret_access_key: s.secret_access_key,
                session_token: s.session_token,
            }),
        };
        Ok(Access {
            region,
            endpoint: backend.options.endpoint.clone(),
            keys,
        })
    }

    pub(crate) async fn resource_driver(
        &self,
        backend: &Backend,
    ) -> Result<Arc<dyn ResourceDriver>> {
        if let Some(d) = &self.resource_driver {
            return Ok(d.clone());
        }
        let access = self.backend_access(backend).await?;
        match backend.provider {
            ResourceProvider::Aws => {
                Ok(Arc::new(aws::AwsDriver::new(&backend.name, &access).await))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Resources by name: their API and outputs (plus `tag <key>` entries).
    pub type Items = BTreeMap<String, (ResourceApi, BTreeMap<String, String>)>;

    /// An IAM user in [`FakeDriver`].
    #[derive(Debug, Clone, Default)]
    pub struct FakeUser {
        pub path: String,
        pub boundary: String,
        pub policy: String,
        /// (key ID, secret)
        pub keys: Vec<(String, String)>,
    }

    /// A backend that keeps buckets, queues and IAM users in memory.
    #[derive(Default)]
    pub struct FakeDriver {
        pub items: Mutex<Items>,
        /// Names `ensure` fails on.
        pub fail: Mutex<BTreeSet<String>>,
        pub users: Mutex<BTreeMap<String, FakeUser>>,
        next_key: Mutex<u32>,
        /// `whoami` calls, one per identity ikit had to reconcile.
        pub iam_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ResourceDriver for FakeDriver {
        async fn ensure(&self, want: &Want<'_>) -> Result<BTreeMap<String, String>> {
            if self.fail.lock().unwrap().contains(want.name) {
                return Err(AppError::Invalid(format!("cannot create {}", want.name)));
            }
            let mut outputs = BTreeMap::new();
            if want.api == ResourceApi::Sqs {
                outputs.insert("url".into(), format!("https://sqs.test/1/{}", want.name));
                outputs.insert("arn".into(), format!("arn:aws:sqs:test:1:{}", want.name));
                if want.options.dead_letter.is_some() {
                    outputs.insert(
                        "dlq_url".into(),
                        format!("https://sqs.test/1/{}-dlq", want.name),
                    );
                }
            }
            for (k, v) in want.tags {
                outputs.insert(format!("tag {k}"), v.clone());
            }
            self.items
                .lock()
                .unwrap()
                .insert(want.name.to_owned(), (want.api, outputs.clone()));
            outputs.retain(|k, _| !k.starts_with("tag "));
            Ok(outputs)
        }

        async fn inspect(
            &self,
            api: ResourceApi,
            id: &str,
        ) -> Result<Option<(String, BTreeMap<String, String>)>> {
            let name = id.rsplit('/').next().unwrap_or(id);
            Ok(self
                .items
                .lock()
                .unwrap()
                .get(name)
                .filter(|(a, _)| *a == api)
                .map(|(_, o)| {
                    let mut o = o.clone();
                    o.retain(|k, _| !k.starts_with("tag "));
                    (name.to_owned(), o)
                }))
        }

        async fn delete(
            &self,
            _api: ResourceApi,
            name: &str,
            _outputs: &BTreeMap<String, String>,
        ) -> Result<()> {
            self.items.lock().unwrap().remove(name);
            Ok(())
        }

        async fn whoami(&self) -> Result<(String, String)> {
            self.iam_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((
                "123456789012".into(),
                "arn:aws:iam::123456789012:user/tofu".into(),
            ))
        }

        async fn ensure_user(&self, user: &IamUser<'_>) -> Result<String> {
            let mut users = self.users.lock().unwrap();
            let u = users
                .entry(user.name.to_owned())
                .or_insert_with(|| FakeUser {
                    path: user.path.to_owned(),
                    boundary: user.boundary.to_owned(),
                    ..Default::default()
                });
            if u.boundary != user.boundary {
                return Err(AppError::Invalid("boundary differs".into()));
            }
            Ok(format!(
                "arn:aws:iam::123456789012:user{}{}",
                user.path, user.name
            ))
        }

        async fn put_user_policy(&self, user: &str, _name: &str, document: &str) -> Result<()> {
            self.users.lock().unwrap().get_mut(user).unwrap().policy = document.to_owned();
            Ok(())
        }

        async fn access_keys(&self, user: &str) -> Result<Vec<String>> {
            Ok(self.users.lock().unwrap()[user]
                .keys
                .iter()
                .map(|(id, _)| id.clone())
                .collect())
        }

        async fn create_access_key(&self, user: &str) -> Result<Keys> {
            let mut users = self.users.lock().unwrap();
            let u = users.get_mut(user).unwrap();
            if u.keys.len() >= 2 {
                return Err(AppError::Invalid("LimitExceeded: two keys already".into()));
            }
            let mut n = self.next_key.lock().unwrap();
            *n += 1;
            let keys = Keys {
                access_key_id: format!("AKIAFAKE{n}"),
                secret_access_key: format!("secret{n}"),
                session_token: None,
            };
            u.keys
                .push((keys.access_key_id.clone(), keys.secret_access_key.clone()));
            Ok(keys)
        }

        async fn delete_access_key(&self, user: &str, id: &str) -> Result<()> {
            if let Some(u) = self.users.lock().unwrap().get_mut(user) {
                u.keys.retain(|(k, _)| k != id);
            }
            Ok(())
        }

        async fn delete_user(&self, user: &str) -> Result<()> {
            self.users.lock().unwrap().remove(user);
            Ok(())
        }

        async fn wait_for_key(&self, _keys: &Keys) -> Result<()> {
            Ok(())
        }
    }
}
