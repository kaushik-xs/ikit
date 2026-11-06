//! Choosing an org's secret store (keychain or encrypted file), this machine's identity, and
//! moving an org's secrets from the keychain into its encrypted file.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use crate::age_store::{
    AgeFileStore, IDENTITY_KEY, LocalIdentity, ORG_SCOPE, RecipientEntry, ScopeInfo,
    recipients_file, secrets_file, uses_secrets_file,
};
use crate::{App, AppError, KeyringStore, Paths, Result, SecretStore};

pub use crate::age_store::{
    IDENTITY_ENV, RecipientEntry as SecretsRecipient, ScopeInfo as SecretsScope,
};

/// Where this machine's private key comes from, besides `IKIT_AGE_IDENTITY` and the keychain.
#[derive(Debug, Clone)]
pub struct IdentityOptions {
    /// An age key file, or an unencrypted SSH private key with its `.pub` beside it.
    pub file: Option<PathBuf>,
    /// A private key given directly (`AGE-SECRET-KEY-…`); wins over everything else.
    pub secret: Option<String>,
    /// Whether to look further (`IKIT_AGE_IDENTITY`, the keychain) when neither is given.
    pub lookup: bool,
}

impl Default for IdentityOptions {
    fn default() -> Self {
        Self {
            file: None,
            secret: None,
            lookup: true,
        }
    }
}

impl IdentityOptions {
    /// This machine's identity, if it has one.
    pub fn resolve(&self) -> Result<Option<LocalIdentity>> {
        if let Some(secret) = &self.secret {
            return Ok(Some(LocalIdentity::from_secret(secret)?));
        }
        if !self.lookup && self.file.is_none() {
            return Ok(None);
        }
        Ok(LocalIdentity::resolve(self.file.as_deref(), &KeyringStore)?)
    }

    /// Options that find no identity, for reading only who can read the secrets.
    fn without_lookup(self) -> Self {
        Self {
            secret: None,
            file: None,
            lookup: false,
        }
    }

    fn require(&self) -> Result<LocalIdentity> {
        self.resolve()?.ok_or_else(|| {
            AppError::Invalid(
                "this machine has no identity yet; create one with `ikit identity create`"
                    .to_owned(),
            )
        })
    }
}

/// The secret store `org` uses: its encrypted file once `ikit org secrets init` has run,
/// otherwise the OS keychain.
pub fn org_secret_store(
    paths: &Paths,
    org: &str,
    identity: &IdentityOptions,
) -> Result<Arc<dyn SecretStore>> {
    let org = paths.org(org);
    if !uses_secrets_file(&org) {
        return Ok(Arc::new(KeyringStore));
    }
    let identity = identity.resolve()?.map(Arc::new);
    Ok(Arc::new(AgeFileStore::new(org, identity)))
}

/// Creates this machine's identity in the keychain and returns its public key.
pub fn identity_create(keychain: &dyn SecretStore, replace: bool) -> Result<String> {
    if !replace && keychain.get(IDENTITY_KEY)?.is_some() {
        return Err(AppError::AlreadyExists(
            "an identity on this machine (see `ikit identity show`; --replace makes a new one)"
                .to_owned(),
        ));
    }
    let (secret, identity) = LocalIdentity::generate();
    keychain.set(IDENTITY_KEY, &secret)?;
    Ok(identity.public)
}

/// A new key pair that is not stored anywhere: (private key, public key). For a recovery
/// key kept offline, or a CI identity.
pub fn identity_generate() -> (String, String) {
    let (secret, identity) = LocalIdentity::generate();
    (secret.as_str().to_owned(), identity.public)
}

/// This machine's public key, if it has an identity.
pub fn identity_public(identity: &IdentityOptions) -> Result<Option<String>> {
    Ok(identity.resolve()?.map(|i| i.public))
}

/// Who can read an org's encrypted secrets.
#[derive(Debug, Clone, Serialize)]
pub struct SecretsInfo {
    pub org: String,
    /// `keychain` or `file`.
    pub store: &'static str,
    /// The org file first, then one per environment that has its own.
    pub scopes: Vec<ScopeInfo>,
    /// This machine's public key, when it has one.
    pub me: Option<String>,
}

/// One problem `ikit org secrets check` found.
#[derive(Debug, Clone, Serialize)]
pub struct SecretsCheck {
    pub scope: String,
    pub problem: String,
}

/// What `ikit org secrets init` moved.
#[derive(Debug, Clone, Serialize)]
pub struct SecretsInitReport {
    pub org: String,
    pub file: PathBuf,
    pub recipients: Vec<RecipientEntry>,
    /// Keys copied from the keychain.
    pub moved: Vec<String>,
    /// Keys the org's state names but the keychain did not have.
    pub missing: Vec<String>,
}

fn file_store(paths: &Paths, org: &str, identity: &IdentityOptions) -> Result<AgeFileStore> {
    let org_paths = paths.org(org);
    if !uses_secrets_file(&org_paths) {
        return Err(AppError::Invalid(format!(
            "org `{org}` keeps its secrets in the keychain; move them to an encrypted file with \
             `ikit org secrets init`"
        )));
    }
    Ok(AgeFileStore::new(
        org_paths,
        identity.resolve()?.map(Arc::new),
    ))
}

pub fn secrets_info(paths: &Paths, org: &str, identity: &IdentityOptions) -> Result<SecretsInfo> {
    let org_paths = paths.org(org);
    let me = identity_public(identity)?;
    if !uses_secrets_file(&org_paths) {
        return Ok(SecretsInfo {
            org: org.to_owned(),
            store: "keychain",
            scopes: vec![],
            me,
        });
    }
    Ok(SecretsInfo {
        org: org.to_owned(),
        store: "file",
        scopes: file_store(paths, org, identity)?.scopes()?,
        me,
    })
}

/// A recipient whose label starts with `recovery` is the offline break-glass key.
fn is_recovery(r: &RecipientEntry) -> bool {
    r.label
        .as_deref()
        .is_some_and(|l| l.starts_with("recovery"))
}

/// A recipient whose label starts with `ci` is an automation key, not a person.
fn is_automation(r: &RecipientEntry) -> bool {
    r.label.as_deref().is_some_and(|l| l.starts_with("ci"))
}

/// Problems that could lock the team out of a secrets file: no recovery key (labelled
/// `recovery…`), or fewer than `min_people` people (labels not `recovery…` or `ci…`).
pub fn secrets_check(paths: &Paths, org: &str, min_people: usize) -> Result<Vec<SecretsCheck>> {
    let store = file_store(paths, org, &IdentityOptions::default().without_lookup())?;
    let mut problems = Vec::new();
    for scope in store.scopes()? {
        let people = scope
            .recipients
            .iter()
            .filter(|r| !is_recovery(r) && !is_automation(r))
            .count();
        if !scope.recipients.iter().any(is_recovery) {
            problems.push(SecretsCheck {
                scope: scope.scope.clone(),
                problem: "no recovery key (grant one with --as recovery)".to_owned(),
            });
        }
        if people < min_people {
            problems.push(SecretsCheck {
                scope: scope.scope.clone(),
                problem: format!(
                    "{people} person can read it; at least {min_people} should, so one lost \
                     laptop does not lock the team out"
                ),
            });
        }
    }
    Ok(problems)
}

/// Gives environment `env` its own secrets file, readable by this machine's key and
/// `extra`. Returns the keys moved out of the org file.
pub fn secrets_split(
    paths: &Paths,
    org: &str,
    identity: &IdentityOptions,
    env: &str,
    extra: Vec<RecipientEntry>,
) -> Result<Vec<String>> {
    let envs = crate::org_config(paths, org)?.envs;
    if !envs.iter().any(|e| e == env) {
        return Err(AppError::Invalid(format!(
            "org `{org}` has no env `{env}` (its envs: {})",
            envs.join(", ")
        )));
    }
    let me = identity.require()?;
    let mut recipients = vec![RecipientEntry {
        key: me.public.clone(),
        label: Some(crate::remote::whoami().0),
    }];
    for e in extra {
        if !recipients.iter().any(|r| r.key == e.key) {
            recipients.push(e);
        }
    }
    Ok(file_store(paths, org, identity)?.split(env, &recipients)?)
}

/// Moves environment `env`'s secrets back into the org file. Returns the keys moved.
pub fn secrets_merge(
    paths: &Paths,
    org: &str,
    identity: &IdentityOptions,
    env: &str,
) -> Result<Vec<String>> {
    Ok(file_store(paths, org, identity)?.merge(env)?)
}

/// Lets `key` read the org's secrets, or only environment `env`'s file. Returns false when it
/// already could.
pub fn secrets_grant(
    paths: &Paths,
    org: &str,
    identity: &IdentityOptions,
    key: &str,
    label: Option<String>,
    env: Option<&str>,
) -> Result<bool> {
    let mut entry = RecipientEntry::parse(key)?;
    entry.label = label.or(entry.label);
    Ok(file_store(paths, org, identity)?.grant(env.unwrap_or(ORG_SCOPE), entry)?)
}

/// Stops the recipient with this key or label from reading future versions of the secrets.
pub fn secrets_revoke(
    paths: &Paths,
    org: &str,
    identity: &IdentityOptions,
    who: &str,
    env: Option<&str>,
) -> Result<RecipientEntry> {
    Ok(file_store(paths, org, identity)?.revoke(env.unwrap_or(ORG_SCOPE), who)?)
}

impl App {
    /// Every secret key the org's state refers to: profile secrets, registry logins,
    /// service secret env vars and hook secrets.
    pub async fn secret_keys(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        for p in self.store.profiles().list(None).await? {
            if !p.secret_keys.is_empty() {
                keys.push(self.profile_secret_key(p.kind, &p.name));
            }
        }
        for login in self.store.registry().logins().await? {
            keys.push(self.registry_secret_key(&login.host));
        }
        for w in self.workload_list(None).await? {
            let detail = self.workload_show(&w.name).await?;
            for s in detail.services.iter().filter(|s| !s.secret_env.is_empty()) {
                keys.push(self.service_secret_key(&w.name, &s.name));
            }
            keys.extend(self.hook_secret_keys(&w.name).await?);
        }
        Ok(keys)
    }

    /// Moves the org's secrets from the keychain into `secrets/org.age`, readable by this
    /// machine's key and `extra` recipients. The keychain entries are left in place until
    /// [`secrets_forget_keychain`](Self::secrets_forget_keychain).
    pub async fn secrets_init(
        &self,
        identity: &IdentityOptions,
        extra: Vec<RecipientEntry>,
    ) -> Result<SecretsInitReport> {
        if uses_secrets_file(&self.org) {
            return Err(AppError::AlreadyExists(format!(
                "an encrypted secrets file for org `{}`",
                self.org.name
            )));
        }
        let me = identity.require()?;
        let mut recipients = vec![RecipientEntry {
            key: me.public.clone(),
            label: Some(crate::remote::whoami().0),
        }];
        for e in extra {
            if !recipients.iter().any(|r| r.key == e.key) {
                recipients.push(e);
            }
        }
        let (mut moved, mut missing, mut secrets) = (vec![], vec![], BTreeMap::new());
        for key in self.secret_keys().await? {
            match self.secrets.get(&key)? {
                Some(value) => {
                    secrets.insert(key.clone(), value);
                    moved.push(key);
                }
                None => missing.push(key),
            }
        }
        let store = AgeFileStore::new(self.org.clone(), Some(Arc::new(me)));
        store.init(&recipients, secrets)?;
        Ok(SecretsInitReport {
            org: self.org.name.clone(),
            file: secrets_file(&self.org),
            recipients,
            moved,
            missing,
        })
    }

    /// Deletes the keychain copies of secrets that the encrypted file now holds. Returns the
    /// keys deleted.
    pub async fn secrets_forget_keychain(&self, keychain: &dyn SecretStore) -> Result<Vec<String>> {
        if !uses_secrets_file(&self.org) {
            return Err(AppError::Invalid(format!(
                "org `{}` still keeps its secrets in the keychain",
                self.org.name
            )));
        }
        let mut deleted = Vec::new();
        for key in self.secret_keys().await? {
            if self.secrets.get(&key)?.is_some() && keychain.get(&key)?.is_some() {
                keychain.delete(&key)?;
                deleted.push(key);
            }
        }
        Ok(deleted)
    }
}

/// Whether the org has started its encrypted secrets file.
pub fn org_uses_secrets_file(paths: &Paths, org: &str) -> bool {
    recipients_file(&paths.org(org)).exists()
}
