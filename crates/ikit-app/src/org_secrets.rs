//! Choosing an org's secret store (keychain or encrypted file), this machine's identity, and
//! moving an org's secrets from the keychain into its encrypted file.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use crate::age_store::{
    AgeFileStore, IDENTITY_KEY, LocalIdentity, RecipientEntry, recipients_file, secrets_file,
    uses_secrets_file,
};
use crate::{App, AppError, KeyringStore, Paths, Result, SecretStore};

pub use crate::age_store::{IDENTITY_ENV, RecipientEntry as SecretsRecipient};

/// Where this machine's private key comes from, besides `IKIT_AGE_IDENTITY` and the keychain.
#[derive(Debug, Clone, Default)]
pub struct IdentityOptions {
    /// An age key file, or an unencrypted SSH private key with its `.pub` beside it.
    pub file: Option<PathBuf>,
    /// A private key given directly (`AGE-SECRET-KEY-…`); wins over everything else.
    pub secret: Option<String>,
}

impl IdentityOptions {
    /// This machine's identity, if it has one.
    pub fn resolve(&self) -> Result<Option<LocalIdentity>> {
        if let Some(secret) = &self.secret {
            return Ok(Some(LocalIdentity::from_secret(secret)?));
        }
        Ok(LocalIdentity::resolve(self.file.as_deref(), &KeyringStore)?)
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
    pub file: Option<PathBuf>,
    pub recipients: Vec<RecipientEntry>,
    /// This machine's public key, when it has one.
    pub me: Option<String>,
    /// Number of secrets, when this machine can read them.
    pub secrets: Option<usize>,
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
            file: None,
            recipients: vec![],
            me,
            secrets: None,
        });
    }
    let store = file_store(paths, org, identity)?;
    Ok(SecretsInfo {
        org: org.to_owned(),
        store: "file",
        file: Some(secrets_file(&org_paths)),
        recipients: store.recipients()?,
        me,
        secrets: store.keys().ok().map(|k| k.len()),
    })
}

/// Lets `key` read the org's secrets. Returns false when it already could.
pub fn secrets_grant(
    paths: &Paths,
    org: &str,
    identity: &IdentityOptions,
    key: &str,
    label: Option<String>,
) -> Result<bool> {
    let mut entry = RecipientEntry::parse(key)?;
    entry.label = label.or(entry.label);
    Ok(file_store(paths, org, identity)?.grant(entry)?)
}

/// Stops the recipient with this key or label from reading future versions of the secrets.
pub fn secrets_revoke(
    paths: &Paths,
    org: &str,
    identity: &IdentityOptions,
    who: &str,
) -> Result<RecipientEntry> {
    Ok(file_store(paths, org, identity)?.revoke(who)?)
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
