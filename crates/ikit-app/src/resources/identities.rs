//! Per-service keys: on backends with `per_service_keys`, an IAM user for each workload service
//! (per provisioning profile), allowed exactly the service's resources, carrying the backend's
//! permissions boundary. Its access key lives in the org's secret store under
//! `org/<org>/workload/<wl>/identity/<service>`, as JSON by profile; the state database only
//! records the user and the key's ID.

use std::collections::{BTreeMap, BTreeSet};

use ikit_core::resource::identity_user_name;
use ikit_core::{Backend, Identity, Resource, ResourceOrigin, ResourceStatus, Workload};
use ikit_state::{IdentityUpdate, NewIdentity};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{IamUser, Keys, iam, owner};
use crate::{App, AppError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityAction {
    /// A new IAM user and key.
    Create,
    /// The user's policy changes: the service's resources changed.
    UpdatePolicy,
    /// A rotated-out key is deleted once the services run with the new one.
    RetireKey,
}

/// An IAM change the next apply makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityPlan {
    pub service: String,
    pub user_name: String,
    pub action: IdentityAction,
    /// Resources (`<service>.<resource>`) its policy will cover.
    pub covers: Vec<String>,
}

/// What `ikit res backend iam-policy` prints.
#[derive(Debug, Clone, Serialize)]
pub struct IdentityPolicies {
    pub backend: String,
    pub account: String,
    /// The provisioning identity (the backend's profile).
    pub caller: String,
    pub boundary_arn: String,
    /// The permissions boundary policy, to create as `boundary_arn`.
    pub boundary: Value,
    /// What the provisioning identity needs.
    pub provisioning: Value,
}

/// An access key as kept in the secret store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredKey {
    access_key_id: String,
    secret_access_key: String,
}

/// One service's resources on per-service-key backends of one provisioning profile.
struct Grant {
    service: String,
    profile: String,
    /// The first such backend; any of them reaches the same account.
    backend: Backend,
    resources: Vec<Resource>,
}

impl Grant {
    fn covers(&self) -> Vec<String> {
        let set: BTreeSet<String> = self.resources.iter().map(Resource::address).collect();
        set.into_iter().collect()
    }
}

fn short_hash(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    digest[..3].iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks the settings per-service keys need.
pub(crate) fn check_per_service(
    backend: &str,
    endpoint: Option<&str>,
    boundary: Option<&str>,
) -> Result<()> {
    if endpoint.is_some() {
        return Err(AppError::Invalid(format!(
            "backend {backend} has an endpoint; per-service keys are AWS IAM users, so they need AWS itself"
        )));
    }
    let Some(b) = boundary else {
        return Err(AppError::Invalid(format!(
            "per-service keys need --boundary <policy ARN>: every IAM user ikit creates carries it \
             (`ikit res backend iam-policy {backend}` prints one to create)"
        )));
    };
    let valid = b
        .strip_prefix("arn:aws:iam::")
        .and_then(|r| r.split_once(":policy/"))
        .is_some_and(|(acct, name)| {
            acct.len() == 12 && acct.bytes().all(|c| c.is_ascii_digit()) && !name.is_empty()
        });
    if !valid {
        return Err(AppError::Invalid(format!(
            "`{b}` is not an IAM policy ARN (arn:aws:iam::<account>:policy/<name>)"
        )));
    }
    Ok(())
}

impl App {
    fn identity_secret_key(&self, workload: &str, service: &str) -> String {
        format!(
            "org/{}/workload/{workload}/identity/{service}",
            self.org.name
        )
    }

    fn identity_keys(&self, workload: &str, service: &str) -> Result<BTreeMap<String, StoredKey>> {
        let key = self.identity_secret_key(workload, service);
        match self.secrets.get(&key)? {
            Some(json) => serde_json::from_str(&json)
                .map_err(|e| AppError::Invalid(format!("secret {key} is not valid JSON: {e}"))),
            None => Ok(BTreeMap::new()),
        }
    }

    fn set_identity_keys(
        &self,
        workload: &str,
        service: &str,
        keys: &BTreeMap<String, StoredKey>,
    ) -> Result<()> {
        let key = self.identity_secret_key(workload, service);
        if keys.is_empty() {
            self.secrets.delete(&key)?;
        } else {
            self.secrets
                .set(&key, &serde_json::to_string(keys).expect("keys serialize"))?;
        }
        Ok(())
    }

    /// The service's own key for `profile`, if ikit holds one and its identity is ready.
    pub(crate) async fn identity_key_for(
        &self,
        wl: &Workload,
        service: &str,
        profile: &str,
    ) -> Result<Option<Keys>> {
        let ready = self
            .store
            .resources()
            .identities(Some(wl.id))
            .await?
            .into_iter()
            .any(|i| {
                i.service == service && i.profile == profile && i.status == ResourceStatus::Ready
            });
        if !ready {
            return Ok(None);
        }
        Ok(self
            .identity_keys(&wl.name, service)?
            .remove(profile)
            .map(|k| Keys {
                access_key_id: k.access_key_id,
                secret_access_key: k.secret_access_key,
                session_token: None,
            }))
    }

    /// What each service's user must cover: its own and shared resources on backends with
    /// per-service keys, grouped by provisioning profile.
    async fn grants(&self, wl: &Workload) -> Result<Vec<Grant>> {
        let rows: Vec<Resource> = self
            .store
            .resources()
            .list(Some(wl.id))
            .await?
            .into_iter()
            .filter(|r| !r.orphaned)
            .collect();
        if rows.is_empty() {
            return Ok(vec![]);
        }
        let services = self.store.workloads().services(wl.id).await?;
        let specs = self.specs_for(&services).await?;
        let backends: BTreeMap<String, Backend> = self
            .resource_backend_list()
            .await?
            .into_iter()
            .map(|b| (b.name.clone(), b))
            .collect();
        let mut grants: BTreeMap<(String, String), Grant> = BTreeMap::new();
        for (service, _, _, spec) in &specs {
            for need in &spec.needs {
                let (o, res) = match need.used() {
                    None => (service.as_str(), need.name.as_str()),
                    Some((s, res)) => match owner(&specs, s) {
                        Some(o) => (o, res),
                        None => continue,
                    },
                };
                let Some(r) = rows.iter().find(|r| r.service == o && r.name == res) else {
                    continue;
                };
                let name = match &r.backend {
                    Some(b) => Some(b.clone()),
                    None => self.bound_backend(&wl.env, r.api).await?,
                };
                let Some(backend) = name.and_then(|n| backends.get(&n)) else {
                    continue;
                };
                if !backend.options.per_service_keys {
                    continue;
                }
                let profile = backend.profile.clone().unwrap_or_default();
                let g = grants
                    .entry((service.clone(), profile.clone()))
                    .or_insert_with(|| Grant {
                        service: service.clone(),
                        profile,
                        backend: backend.clone(),
                        resources: vec![],
                    });
                if !g.resources.iter().any(|x| x.id == r.id) {
                    g.resources.push(r.clone());
                }
            }
        }
        Ok(grants.into_values().collect())
    }

    /// IAM changes the next apply makes.
    pub(crate) async fn identity_plans(&self, wl: &Workload) -> Result<Vec<IdentityPlan>> {
        let idents = self.store.resources().identities(Some(wl.id)).await?;
        let mut plans = Vec::new();
        for g in self.grants(wl).await? {
            let covers = g.covers();
            let existing = idents
                .iter()
                .find(|i| i.service == g.service && i.profile == g.profile);
            let user_name = existing
                .map(|i| i.user_name.clone())
                .unwrap_or_else(|| identity_user_name(&wl.name, &g.service, short_hash));
            let action = match existing {
                None => Some(IdentityAction::Create),
                Some(i) if i.status == ResourceStatus::Pending => Some(IdentityAction::Create),
                Some(i) if i.covers != covers => Some(IdentityAction::UpdatePolicy),
                Some(i) if i.retiring_key_id.is_some() => Some(IdentityAction::RetireKey),
                Some(_) => None,
            };
            if let Some(action) = action {
                plans.push(IdentityPlan {
                    service: g.service,
                    user_name,
                    action,
                    covers,
                });
            }
        }
        Ok(plans)
    }

    /// Makes sure every service on a per-service-key backend has its user, policy and a key
    /// ikit holds. Returns whether any key was created (the compose files change).
    pub(crate) async fn ensure_identities(&self, wl: &Workload) -> Result<bool> {
        let mut changed = false;
        let idents = self.store.resources().identities(Some(wl.id)).await?;
        for g in self.grants(wl).await? {
            // In place already: no IAM calls, so deploying needs no provisioning credentials.
            let done = idents.iter().any(|i| {
                i.service == g.service
                    && i.profile == g.profile
                    && i.status == ResourceStatus::Ready
                    && i.covers == g.covers()
            }) && self
                .identity_keys(&wl.name, &g.service)?
                .get(&g.profile)
                .is_some_and(|k| {
                    idents.iter().any(|i| {
                        i.service == g.service
                            && i.access_key_id.as_deref() == Some(k.access_key_id.as_str())
                    })
                });
            if done {
                continue;
            }
            let driver = self.resource_driver(&g.backend).await?;
            let boundary = g.backend.options.boundary.clone().ok_or_else(|| {
                AppError::Invalid(format!(
                    "backend {} has per-service keys but no boundary",
                    g.backend.name
                ))
            })?;
            let (account, _) = driver.whoami().await?;
            let repo = self.store.resources();
            let mut ident = match repo
                .identities(Some(wl.id))
                .await?
                .into_iter()
                .find(|i| i.service == g.service && i.profile == g.profile)
            {
                Some(i) => i,
                None => {
                    let name = identity_user_name(&wl.name, &g.service, short_hash);
                    repo.insert_identity(
                        wl.id,
                        NewIdentity {
                            service: &g.service,
                            profile: &g.profile,
                            user_name: &name,
                        },
                    )
                    .await?
                }
            };
            let tags = BTreeMap::from([
                ("ikit:org".to_owned(), self.org.name.clone()),
                ("ikit:workload".to_owned(), wl.name.clone()),
                ("ikit:service".to_owned(), g.service.clone()),
            ]);
            let path = iam::user_path(&self.org.name);
            let arn = driver
                .ensure_user(&IamUser {
                    name: &ident.user_name,
                    path: &path,
                    boundary: &boundary,
                    tags: &tags,
                })
                .await?;
            let region = self.backend_region(&g.backend).await?;
            let refs: Vec<&Resource> = g.resources.iter().collect();
            let policy = iam::service_policy(&refs, &region, &account);
            driver
                .put_user_policy(&ident.user_name, iam::POLICY_NAME, &policy.to_string())
                .await?;

            let mut stored = self.identity_keys(&wl.name, &g.service)?;
            let existing = driver.access_keys(&ident.user_name).await?;
            let current = stored
                .get(&g.profile)
                .filter(|k| existing.contains(&k.access_key_id))
                .map(|k| k.access_key_id.clone());
            let key_id = match current {
                Some(id) => id,
                None => {
                    // Keys whose secret ikit doesn't hold are of no use; IAM allows two.
                    for id in existing
                        .iter()
                        .filter(|id| ident.retiring_key_id.as_ref() != Some(*id))
                    {
                        driver.delete_access_key(&ident.user_name, id).await?;
                    }
                    let keys = driver.create_access_key(&ident.user_name).await?;
                    // Keep it before waiting, so a timeout doesn't lose the secret.
                    stored.insert(
                        g.profile.clone(),
                        StoredKey {
                            access_key_id: keys.access_key_id.clone(),
                            secret_access_key: keys.secret_access_key.clone(),
                        },
                    );
                    self.set_identity_keys(&wl.name, &g.service, &stored)?;
                    driver.wait_for_key(&keys).await?;
                    changed = true;
                    keys.access_key_id
                }
            };
            ident.account = Some(account);
            ident.user_arn = Some(arn);
            ident.access_key_id = Some(key_id);
            ident.covers = g.covers();
            ident.status = ResourceStatus::Ready;
            repo.update_identity(ident.id, IdentityUpdate::from(&ident))
                .await?;
        }
        Ok(changed)
    }

    /// The driver that manages `ident`: a backend with its provisioning profile.
    async fn identity_driver(
        &self,
        ident: &Identity,
    ) -> Result<std::sync::Arc<dyn super::ResourceDriver>> {
        let backends = self.resource_backend_list().await?;
        let backend = backends
            .iter()
            .filter(|b| b.profile.clone().unwrap_or_default() == ident.profile)
            .max_by_key(|b| b.options.per_service_keys)
            .ok_or_else(|| {
                AppError::Invalid(format!(
                    "no backend uses profile `{}` any more, so IAM user {} can't be reached",
                    ident.profile, ident.user_name
                ))
            })?;
        self.resource_driver(backend).await
    }

    /// Deletes keys rotated out, once the services run with their new ones.
    pub(crate) async fn retire_keys(&self, wl: &Workload) -> Result<()> {
        for mut ident in self.store.resources().identities(Some(wl.id)).await? {
            let Some(old) = ident.retiring_key_id.clone() else {
                continue;
            };
            self.identity_driver(&ident)
                .await?
                .delete_access_key(&ident.user_name, &old)
                .await?;
            ident.retiring_key_id = None;
            self.store
                .resources()
                .update_identity(ident.id, IdentityUpdate::from(&ident))
                .await?;
        }
        Ok(())
    }

    /// IAM users of one workload, or of every workload.
    pub async fn identities_list(&self, workload: Option<&str>) -> Result<Vec<Identity>> {
        let id = match workload {
            Some(w) => Some(self.workload_get(w).await?.id),
            None => None,
        };
        Ok(self.store.resources().identities(id).await?)
    }

    /// Gives the service a new key; the old one keeps working until the next apply has
    /// redeployed the service, which then deletes it.
    pub async fn identity_rotate(&self, workload: &str, service: &str) -> Result<Vec<Identity>> {
        let idents: Vec<Identity> = self
            .identities_list(Some(workload))
            .await?
            .into_iter()
            .filter(|i| i.service == service && i.status == ResourceStatus::Ready && !i.orphaned)
            .collect();
        if idents.is_empty() {
            return Err(AppError::NotFound(format!(
                "IAM user for `{service}` in {workload} (only services on backends with per-service keys have one, after an apply)"
            )));
        }
        let mut stored = self.identity_keys(workload, service)?;
        let mut out = Vec::new();
        for mut ident in idents {
            if ident.retiring_key_id.is_some() {
                return Err(AppError::Invalid(format!(
                    "{}'s previous key is still being retired; run `ikit apply` first",
                    ident.user_name
                )));
            }
            let driver = self.identity_driver(&ident).await?;
            let existing = driver.access_keys(&ident.user_name).await?;
            for id in existing
                .iter()
                .filter(|id| ident.access_key_id.as_ref() != Some(*id))
            {
                driver.delete_access_key(&ident.user_name, id).await?;
            }
            let keys = driver.create_access_key(&ident.user_name).await?;
            stored.insert(
                ident.profile.clone(),
                StoredKey {
                    access_key_id: keys.access_key_id.clone(),
                    secret_access_key: keys.secret_access_key.clone(),
                },
            );
            self.set_identity_keys(workload, service, &stored)?;
            driver.wait_for_key(&keys).await?;
            ident.retiring_key_id = ident.access_key_id.take();
            ident.access_key_id = Some(keys.access_key_id);
            ident.rotated_at = Some(chrono::Utc::now());
            self.store
                .resources()
                .update_identity(ident.id, IdentityUpdate::from(&ident))
                .await?;
            out.push(ident);
        }
        Ok(out)
    }

    /// Deletes the IAM user of `ident` (unless `keep`) and forgets it and its key.
    async fn forget_identity(&self, workload: &str, ident: &Identity, keep: bool) -> Result<bool> {
        let delete = !keep && ident.status == ResourceStatus::Ready;
        if delete {
            self.identity_driver(ident)
                .await?
                .delete_user(&ident.user_name)
                .await?;
        }
        let mut stored = self.identity_keys(workload, &ident.service)?;
        stored.remove(&ident.profile);
        self.set_identity_keys(workload, &ident.service, &stored)?;
        self.store.resources().delete_identity(ident.id).await?;
        Ok(delete)
    }

    /// Removes a removed service's IAM users. Returns whether any was deleted in AWS.
    pub async fn identity_remove(&self, workload: &str, service: &str, keep: bool) -> Result<bool> {
        let idents: Vec<Identity> = self
            .identities_list(Some(workload))
            .await?
            .into_iter()
            .filter(|i| i.service == service)
            .collect();
        if idents.is_empty() {
            return Err(AppError::NotFound(format!(
                "IAM user for `{service}` in {workload}"
            )));
        }
        if idents.iter().any(|i| !i.orphaned) {
            return Err(AppError::InUse {
                what: format!("the IAM user of `{service}`"),
                by: vec![format!(
                    "service `{service}` (remove it first with `ikit svc rm {service}`)"
                )],
            });
        }
        let mut deleted = false;
        for i in &idents {
            deleted |= self.forget_identity(workload, i, keep).await?;
        }
        Ok(deleted)
    }

    /// Forgets (deleting unless `keep`) the IAM users of a removed service that owns no
    /// resources any more.
    pub(crate) async fn forget_service_identities(
        &self,
        wl: &Workload,
        service: &str,
        keep: bool,
    ) -> Result<()> {
        let still_here = self
            .store
            .workloads()
            .services(wl.id)
            .await?
            .iter()
            .any(|s| s.name == service);
        let owns = self
            .store
            .resources()
            .list(Some(wl.id))
            .await?
            .iter()
            .any(|r| r.service == service);
        if still_here || owns {
            return Ok(());
        }
        for i in self
            .store
            .resources()
            .identities(Some(wl.id))
            .await?
            .iter()
            .filter(|i| i.service == service)
        {
            self.forget_identity(&wl.name, i, keep).await?;
        }
        Ok(())
    }

    /// Every IAM user of `wl`, deleted unless `keep`.
    pub(crate) async fn forget_identities(&self, wl: &Workload, keep: bool) -> Result<()> {
        for i in self.store.resources().identities(Some(wl.id)).await? {
            self.forget_identity(&wl.name, &i, keep).await?;
        }
        Ok(())
    }

    /// IAM users ikit created for `wl` that still exist.
    pub(crate) async fn created_identities(&self, wl: &Workload) -> Result<Vec<Identity>> {
        Ok(self
            .store
            .resources()
            .identities(Some(wl.id))
            .await?
            .into_iter()
            .filter(|i| i.status == ResourceStatus::Ready)
            .collect())
    }

    /// Marks identities of removed services orphaned, and of returned ones not.
    pub(crate) async fn sync_identities(&self, wl: &Workload) -> Result<()> {
        let services: BTreeSet<String> = self
            .store
            .workloads()
            .services(wl.id)
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect();
        for mut i in self.store.resources().identities(Some(wl.id)).await? {
            let orphaned = !services.contains(&i.service);
            if orphaned != i.orphaned {
                i.orphaned = orphaned;
                self.store
                    .resources()
                    .update_identity(i.id, IdentityUpdate::from(&i))
                    .await?;
            }
        }
        Ok(())
    }

    /// The permissions boundary and provisioning policies a backend's per-service keys need,
    /// for its account.
    pub async fn identity_policies(&self, backend: &str) -> Result<IdentityPolicies> {
        let b = self.resource_backend_show(backend).await?;
        let (account, caller) = self.resource_driver(&b).await?.whoami().await?;
        let config = self.config()?;
        let boundary_arn = b.options.boundary.clone().unwrap_or_else(|| {
            format!(
                "arn:aws:iam::{account}:policy/ikit-{}-boundary",
                self.org.name
            )
        });
        // Adopted resources outside the prefix must be in the boundary too.
        let profile = b.profile.clone();
        let backends: BTreeSet<String> = self
            .resource_backend_list()
            .await?
            .into_iter()
            .filter(|x| x.profile == profile)
            .map(|x| x.name)
            .collect();
        let extra: Vec<Resource> = self
            .store
            .resources()
            .list(None)
            .await?
            .into_iter()
            .filter(|r| {
                r.origin == ResourceOrigin::Adopted
                    && r.backend.as_ref().is_some_and(|x| backends.contains(x))
                    && !r.external_id.starts_with(&format!("{}-", config.prefix))
            })
            .collect();
        let refs: Vec<&Resource> = extra.iter().collect();
        Ok(IdentityPolicies {
            backend: b.name,
            boundary: iam::boundary_policy(&config.prefix, &account, &refs),
            provisioning: iam::provisioning_policy(
                &self.org.name,
                &config.prefix,
                &account,
                &boundary_arn,
            ),
            account,
            caller,
            boundary_arn,
        })
    }

    /// Adopted resources a service's user would need but the boundary (as `iam-policy` prints
    /// it for the org's prefix) doesn't name: they must be added to it by hand.
    pub(crate) async fn boundary_warnings(&self, wl: &Workload) -> Result<Vec<String>> {
        let prefix = format!("{}-", self.config()?.prefix);
        let mut warnings = Vec::new();
        for g in self.grants(wl).await? {
            for r in g
                .resources
                .iter()
                .filter(|r| !r.external_id.starts_with(&prefix))
            {
                warnings.push(format!(
                    "{} ({}) is outside the `{prefix}*` names the permissions boundary allows; \
                     `{}`'s key can't reach it until the boundary lists it (see `ikit res backend iam-policy {}`)",
                    r.address(),
                    r.external_id,
                    g.service,
                    g.backend.name
                ));
            }
        }
        Ok(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_service_keys_need_aws_and_a_boundary() {
        let arn = "arn:aws:iam::123456789012:policy/ikit-acme-boundary";
        assert!(check_per_service("b", None, Some(arn)).is_ok());
        assert!(check_per_service("b", None, None).is_err());
        assert!(check_per_service("b", Some("http://minio:9000"), Some(arn)).is_err());
        for bad in [
            "ikit-acme-boundary",
            "arn:aws:iam::123:policy/x",
            "arn:aws:iam::123456789012:role/x",
            "arn:aws:iam::123456789012:policy/",
        ] {
            assert!(check_per_service("b", None, Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn long_user_names_are_cut_with_a_hash() {
        let short = identity_user_name("wl-acme-dev-002", "simulation-service", short_hash);
        assert_eq!(short, "wl-acme-dev-002-simulation-service");
        let a = identity_user_name("wl-acme-dev-002", &"a".repeat(60), short_hash);
        let b = identity_user_name(
            "wl-acme-dev-002",
            &format!("{}b", "a".repeat(59)),
            short_hash,
        );
        assert_eq!(a.len(), 64);
        assert_ne!(a, b, "different services stay different");
    }
}
