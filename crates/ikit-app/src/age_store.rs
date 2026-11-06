//! Secrets in an age-encrypted file inside the org folder, so they travel with the org's
//! remote state and any listed teammate can read them. Nothing leaves the machine in clear.
//!
//! ```text
//! orgs/<org>/recipients/org.txt   public keys that can read it, one per line: `age1… # alice`
//! orgs/<org>/secrets/org.age      {"format":1,"secrets":{"org/<org>/profile/…": "…"}}
//! ```
//!
//! An org uses the file once `recipients/org.txt` exists (`ikit org secrets init`); until then
//! it uses the OS keychain. Keys are the same as the keychain's, so callers do not change.
//!
//! An environment can have its own file for a smaller group (`ikit org secrets split prod`):
//! `secrets/prod.age` with `recipients/prod.txt` then holds the secrets of `prod` workloads
//! and their hooks, and `org.age` everything else.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use age::secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::paths::OrgPaths;
use crate::secrets::{SecretError, SecretStore};

/// The secrets file every org has; environments may add their own.
pub const ORG_SCOPE: &str = "org";
/// Keychain account holding this machine's private key.
pub const IDENTITY_KEY: &str = "identity/default";
/// Environment variable holding a private key (`AGE-SECRET-KEY-…`), e.g. in CI.
pub const IDENTITY_ENV: &str = "IKIT_AGE_IDENTITY";

pub fn recipients_file(org: &OrgPaths) -> PathBuf {
    recipients_path(org, ORG_SCOPE)
}

pub fn secrets_file(org: &OrgPaths) -> PathBuf {
    secrets_path(org, ORG_SCOPE)
}

/// `recipients/<scope>.txt`
pub fn recipients_path(org: &OrgPaths, scope: &str) -> PathBuf {
    org.dir.join("recipients").join(format!("{scope}.txt"))
}

/// `secrets/<scope>.age`
pub fn secrets_path(org: &OrgPaths, scope: &str) -> PathBuf {
    org.dir.join("secrets").join(format!("{scope}.age"))
}

/// The environment whose workload a secret key belongs to: `org/<org>/workload/<wl>/…` and
/// `org/<org>/hook/<wl>/…`.
fn key_env(key: &str) -> Option<String> {
    let mut parts = key.split('/');
    let (Some("org"), Some(_), Some("workload" | "hook"), Some(wl)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    wl.parse::<ikit_core::workload::WorkloadName>()
        .ok()
        .map(|n| n.env)
}

/// Whether the org keeps its secrets in the encrypted file rather than the keychain.
pub fn uses_secrets_file(org: &OrgPaths) -> bool {
    recipients_file(org).exists()
}

fn err(message: impl Into<String>) -> SecretError {
    SecretError::new(message)
}

/// One line of a recipients file: a public key and who it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecipientEntry {
    /// `age1…` or `ssh-ed25519 AAAA…` (without the SSH comment).
    pub key: String,
    pub label: Option<String>,
}

impl RecipientEntry {
    /// Parses `age1… # label` or `ssh-ed25519 AAAA… [comment] [# label]`.
    pub fn parse(line: &str) -> Result<Self, SecretError> {
        let (key_part, label) = match line.split_once('#') {
            Some((k, l)) => (
                k.trim(),
                Some(l.trim().to_owned()).filter(|l| !l.is_empty()),
            ),
            None => (line.trim(), None),
        };
        let mut words = key_part.split_whitespace();
        let first = words.next().ok_or_else(|| err("empty recipient"))?;
        let key = if first.starts_with("ssh-") {
            let body = words
                .next()
                .ok_or_else(|| err(format!("incomplete SSH key `{first}`")))?;
            format!("{first} {body}")
        } else {
            first.to_owned()
        };
        recipient(&key)?;
        Ok(Self { key, label })
    }

    fn line(&self) -> String {
        match &self.label {
            Some(l) => format!("{} # {l}", self.key),
            None => self.key.clone(),
        }
    }
}

fn recipient(key: &str) -> Result<Box<dyn age::Recipient + Send>, SecretError> {
    if key.starts_with("age1") {
        age::x25519::Recipient::from_str(key)
            .map(|r| Box::new(r) as _)
            .map_err(|e| err(format!("invalid age public key `{key}`: {e}")))
    } else if key.starts_with("ssh-") {
        age::ssh::Recipient::from_str(key)
            .map(|r| Box::new(r) as _)
            .map_err(|e| err(format!("invalid SSH public key `{key}`: {e:?}")))
    } else {
        Err(err(format!(
            "`{key}` is not a public key: use an age key (age1…) or an ssh-ed25519/ssh-rsa key"
        )))
    }
}

pub fn read_recipients(path: &Path) -> Result<Vec<RecipientEntry>, SecretError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| err(format!("reading {}: {e}", path.display())))?;
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(RecipientEntry::parse)
        .collect()
}

fn write_recipients(path: &Path, entries: &[RecipientEntry]) -> Result<(), SecretError> {
    let mut text = String::from(
        "# Public keys that can read secrets/org.age (ikit org secrets grant|revoke)\n",
    );
    for e in entries {
        text.push_str(&e.line());
        text.push('\n');
    }
    write_file(path, text.as_bytes())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    let io = |e: std::io::Error| err(format!("writing {}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// Every secrets file's recipients, by scope (`org`, `prod`, …), as the org folder lists them.
pub(crate) fn all_recipients(org: &OrgPaths) -> BTreeMap<String, Vec<RecipientEntry>> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(org.dir.join("recipients")) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(scope) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".txt"))
        else {
            continue;
        };
        if let Ok(list) = read_recipients(&path) {
            out.insert(scope.to_owned(), list);
        }
    }
    out
}

/// Lines describing who gained or lost access between two [`all_recipients`] readings.
pub(crate) fn recipient_changes(
    before: &BTreeMap<String, Vec<RecipientEntry>>,
    after: &BTreeMap<String, Vec<RecipientEntry>>,
) -> Vec<String> {
    let name = |r: &RecipientEntry| match &r.label {
        Some(l) => format!("{l} ({})", short_key(&r.key)),
        None => short_key(&r.key),
    };
    let mut lines = Vec::new();
    let scopes: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for scope in scopes {
        let (old, new) = (
            before.get(scope).map(Vec::as_slice).unwrap_or_default(),
            after.get(scope).map(Vec::as_slice).unwrap_or_default(),
        );
        for r in new.iter().filter(|r| !old.iter().any(|o| o.key == r.key)) {
            lines.push(format!("{} can now read {scope} secrets", name(r)));
        }
        for r in old.iter().filter(|r| !new.iter().any(|n| n.key == r.key)) {
            lines.push(format!("{} can no longer read {scope} secrets", name(r)));
        }
    }
    lines
}

/// `age1abcd…wxyz` / `ssh-ed25519 AAAA…wxyz`, enough to tell keys apart.
fn short_key(key: &str) -> String {
    let (kind, body) = key.split_once(' ').map_or(("", key), |(k, b)| (k, b));
    let short = if body.len() > 14 {
        format!("{}…{}", &body[..8], &body[body.len() - 4..])
    } else {
        body.to_owned()
    };
    if kind.is_empty() {
        short
    } else {
        format!("{kind} {short}")
    }
}

/// This machine's private key and the public key it pairs with.
pub struct LocalIdentity {
    identity: Box<dyn age::Identity + Send + Sync>,
    /// As it appears in recipients files.
    pub public: String,
}

impl std::fmt::Debug for LocalIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalIdentity")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl LocalIdentity {
    /// A new X25519 key pair: (private key to keep, identity).
    pub fn generate() -> (Zeroizing<String>, Self) {
        let id = age::x25519::Identity::generate();
        let secret = Zeroizing::new(id.to_string().expose_secret().to_owned());
        let public = id.to_public().to_string();
        (
            secret,
            Self {
                identity: Box::new(id),
                public,
            },
        )
    }

    /// From an `AGE-SECRET-KEY-…` string.
    pub fn from_secret(secret: &str) -> Result<Self, SecretError> {
        let id = age::x25519::Identity::from_str(secret.trim())
            .map_err(|_| err("not a valid age private key (AGE-SECRET-KEY-…)"))?;
        let public = id.to_public().to_string();
        Ok(Self {
            identity: Box::new(id),
            public,
        })
    }

    /// From an age key file, or an unencrypted SSH private key with its `.pub` beside it.
    pub fn from_file(path: &Path) -> Result<Self, SecretError> {
        let text = Zeroizing::new(
            std::fs::read_to_string(path)
                .map_err(|e| err(format!("reading {}: {e}", path.display())))?,
        );
        if let Some(line) = text
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("AGE-SECRET-KEY-"))
        {
            return Self::from_secret(line);
        }
        let parsed =
            age::ssh::Identity::from_buffer(text.as_bytes(), Some(path.display().to_string()))
                .map_err(|e| err(format!("{}: {e}", path.display())))?;
        match parsed {
            age::ssh::Identity::Unencrypted(_) => {}
            age::ssh::Identity::Encrypted(_) => {
                return Err(err(format!(
                    "{} is protected by a passphrase, which ikit cannot prompt for; use an age \
                     identity (`ikit identity create`) instead",
                    path.display()
                )));
            }
            age::ssh::Identity::Unsupported(k) => {
                return Err(err(format!(
                    "{}: unsupported SSH key: {k:?}",
                    path.display()
                )));
            }
        }
        let pub_path = PathBuf::from(format!("{}.pub", path.display()));
        let pub_line = std::fs::read_to_string(&pub_path).map_err(|e| {
            err(format!(
                "reading {} (the public half of {}): {e}",
                pub_path.display(),
                path.display()
            ))
        })?;
        let public = RecipientEntry::parse(&pub_line)?.key;
        Ok(Self {
            identity: Box::new(parsed),
            public,
        })
    }

    /// The identity to use: `file`, else `IKIT_AGE_IDENTITY`, else the keychain's.
    /// `None` when there is none.
    pub fn resolve(
        file: Option<&Path>,
        keychain: &dyn SecretStore,
    ) -> Result<Option<Self>, SecretError> {
        if let Some(path) = file {
            return Self::from_file(path).map(Some);
        }
        if let Ok(secret) = std::env::var(IDENTITY_ENV) {
            return Self::from_secret(&secret)
                .map(Some)
                .map_err(|e| err(format!("{IDENTITY_ENV}: {e}")));
        }
        match keychain.get(IDENTITY_KEY)? {
            Some(secret) => Self::from_secret(&Zeroizing::new(secret)).map(Some),
            None => Ok(None),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Plaintext {
    format: u32,
    secrets: BTreeMap<String, String>,
}

type Secrets = BTreeMap<String, Zeroizing<String>>;

/// One secrets file and who can read it, as summarized by [`AgeFileStore::scopes`].
#[derive(Debug, Clone, Serialize)]
pub struct ScopeInfo {
    /// `org`, or an environment.
    pub scope: String,
    pub file: PathBuf,
    pub recipients: Vec<RecipientEntry>,
    /// Number of secrets, when this machine can read them.
    pub secrets: Option<usize>,
}

/// Secrets in `secrets/<scope>.age`, each file decrypted on first use and kept in memory only.
pub struct AgeFileStore {
    org: OrgPaths,
    identity: Option<Arc<LocalIdentity>>,
    cache: Mutex<BTreeMap<String, Secrets>>,
}

impl AgeFileStore {
    pub fn new(org: OrgPaths, identity: Option<Arc<LocalIdentity>>) -> Self {
        Self {
            org,
            identity,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    fn identity(&self) -> Result<&LocalIdentity, SecretError> {
        self.identity.as_deref().ok_or_else(|| {
            err(format!(
                "org `{}` keeps its secrets in an encrypted file and this machine has no \
                 identity; run `ikit identity create` and ask a teammate to run \
                 `ikit org secrets grant <your public key>`",
                self.org.name
            ))
        })
    }

    fn has_scope(&self, scope: &str) -> bool {
        recipients_path(&self.org, scope).exists()
    }

    fn require_scope(&self, scope: &str) -> Result<(), SecretError> {
        if self.has_scope(scope) {
            Ok(())
        } else {
            Err(err(format!(
                "org `{}` has no `{scope}` secrets file",
                self.org.name
            )))
        }
    }

    /// The file a key belongs in: its workload's environment file when there is one.
    fn scope_for(&self, key: &str) -> String {
        match key_env(key) {
            Some(env) if self.has_scope(&env) => env,
            _ => ORG_SCOPE.to_owned(),
        }
    }

    pub fn recipients(&self, scope: &str) -> Result<Vec<RecipientEntry>, SecretError> {
        read_recipients(&recipients_path(&self.org, scope))
    }

    /// Every secrets file, `org` first.
    pub fn scopes(&self) -> Result<Vec<ScopeInfo>, SecretError> {
        let mut names: Vec<String> = all_recipients(&self.org).into_keys().collect();
        names.sort_by_key(|n| (n != ORG_SCOPE, n.clone()));
        names
            .into_iter()
            .map(|scope| {
                Ok(ScopeInfo {
                    file: secrets_path(&self.org, &scope),
                    recipients: self.recipients(&scope)?,
                    secrets: self.with_secrets(&scope, |s| s.len()).ok(),
                    scope,
                })
            })
            .collect()
    }

    fn with_secrets<T>(
        &self,
        scope: &str,
        f: impl FnOnce(&mut Secrets) -> T,
    ) -> Result<T, SecretError> {
        let mut cache = self.cache.lock().expect("secrets cache");
        if !cache.contains_key(scope) {
            let loaded = self.decrypt(scope)?;
            cache.insert(scope.to_owned(), loaded);
        }
        Ok(f(cache.get_mut(scope).expect("loaded")))
    }

    fn decrypt(&self, scope: &str) -> Result<Secrets, SecretError> {
        let path = secrets_path(&self.org, scope);
        if !path.exists() {
            return Ok(Secrets::new());
        }
        let identity = self.identity()?;
        let bytes =
            std::fs::read(&path).map_err(|e| err(format!("reading {}: {e}", path.display())))?;
        let decryptor = age::Decryptor::new(bytes.as_slice())
            .map_err(|e| err(format!("{}: {e}", path.display())))?;
        let mut reader = decryptor
            .decrypt(std::iter::once(
                identity.identity.as_ref() as &dyn age::Identity
            ))
            .map_err(|e| match e {
                age::DecryptError::NoMatchingKeys => err(format!(
                    "your key {} cannot read {}; ask a teammate to run \
                     `ikit org secrets grant {}{}`",
                    identity.public,
                    path.display(),
                    identity.public,
                    if scope == ORG_SCOPE {
                        String::new()
                    } else {
                        format!(" --env {scope}")
                    }
                )),
                e => err(format!("{}: {e}", path.display())),
            })?;
        let mut json = Zeroizing::new(Vec::new());
        reader
            .read_to_end(&mut json)
            .map_err(|e| err(format!("{}: {e}", path.display())))?;
        let plain: Plaintext = serde_json::from_slice(&json)
            .map_err(|e| err(format!("{}: not a secrets file: {e}", path.display())))?;
        if plain.format != 1 {
            return Err(err(format!(
                "{}: format {} needs a newer ikit",
                path.display(),
                plain.format
            )));
        }
        Ok(plain
            .secrets
            .into_iter()
            .map(|(k, v)| (k, Zeroizing::new(v)))
            .collect())
    }

    /// Encrypts `secrets` to `recipients` as the `scope` file. Refused unless this machine's
    /// key is in `writers` (the recipients before the change): only someone who can already
    /// read the secrets may change them or who can read them.
    fn encrypt(
        &self,
        scope: &str,
        secrets: &Secrets,
        recipients: &[RecipientEntry],
        writers: &[RecipientEntry],
    ) -> Result<(), SecretError> {
        let me = &self.identity()?.public;
        if !writers.iter().any(|r| &r.key == me) {
            return Err(err(format!(
                "your key {me} is not a recipient of org `{}`'s {scope} secrets, so you cannot \
                 change them",
                self.org.name
            )));
        }
        let keys = recipients
            .iter()
            .map(|r| recipient(&r.key))
            .collect::<Result<Vec<_>, _>>()?;
        let plain = Plaintext {
            format: 1,
            secrets: secrets
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().to_owned()))
                .collect(),
        };
        let json = Zeroizing::new(serde_json::to_vec(&plain).expect("secrets serialize"));
        let encryptor =
            age::Encryptor::with_recipients(keys.iter().map(|r| r.as_ref() as &dyn age::Recipient))
                .map_err(|e| err(format!("encrypting secrets: {e}")))?;
        let mut out = Vec::new();
        let mut writer = encryptor
            .wrap_output(&mut out)
            .map_err(|e| err(format!("encrypting secrets: {e}")))?;
        writer
            .write_all(&json)
            .and_then(|()| writer.finish().map(|_| ()))
            .map_err(|e| err(format!("encrypting secrets: {e}")))?;
        write_file(&secrets_path(&self.org, scope), &out)?;
        self.cache
            .lock()
            .expect("secrets cache")
            .insert(scope.to_owned(), secrets.clone());
        Ok(())
    }

    /// Starts the org's encrypted file with `recipients` (which must include this machine's
    /// key) and `secrets`.
    pub fn init(
        &self,
        recipients: &[RecipientEntry],
        secrets: BTreeMap<String, String>,
    ) -> Result<(), SecretError> {
        if uses_secrets_file(&self.org) {
            return Err(err(format!(
                "org `{}` already keeps its secrets in {}",
                self.org.name,
                secrets_file(&self.org).display()
            )));
        }
        let secrets: Secrets = secrets
            .into_iter()
            .map(|(k, v)| (k, Zeroizing::new(v)))
            .collect();
        self.encrypt(ORG_SCOPE, &secrets, recipients, recipients)?;
        write_recipients(&recipients_file(&self.org), recipients)
    }

    /// Adds a recipient to `scope` and re-encrypts. Returns false when it was already listed.
    pub fn grant(&self, scope: &str, entry: RecipientEntry) -> Result<bool, SecretError> {
        self.require_scope(scope)?;
        let mut recipients = self.recipients(scope)?;
        if recipients.iter().any(|r| r.key == entry.key) {
            return Ok(false);
        }
        recipients.push(entry);
        self.rewrite(scope, &recipients)?;
        Ok(true)
    }

    /// Removes the recipient of `scope` whose key or label is `who` and re-encrypts.
    pub fn revoke(&self, scope: &str, who: &str) -> Result<RecipientEntry, SecretError> {
        self.require_scope(scope)?;
        let mut recipients = self.recipients(scope)?;
        let who = who.trim();
        let i = recipients
            .iter()
            .position(|r| r.key == who || r.label.as_deref() == Some(who))
            .ok_or_else(|| err(format!("`{who}` is not a recipient of the {scope} secrets")))?;
        if recipients.len() == 1 {
            return Err(err(
                "that is the last recipient; nobody could read the secrets after revoking it",
            ));
        }
        let removed = recipients.remove(i);
        self.rewrite(scope, &recipients)?;
        Ok(removed)
    }

    /// Re-encrypts `scope` to a changed recipient list. The secrets are decrypted first, so
    /// this works even when the change removes this machine's own key.
    fn rewrite(&self, scope: &str, recipients: &[RecipientEntry]) -> Result<(), SecretError> {
        let before = self.recipients(scope)?;
        let secrets = self.with_secrets(scope, |s| s.clone())?;
        self.encrypt(scope, &secrets, recipients, &before)?;
        write_recipients(&recipients_path(&self.org, scope), recipients)
    }

    /// Gives environment `env` its own secrets file readable by `recipients`, moving its
    /// workloads' and hooks' secrets out of the org file. Returns the keys moved.
    pub fn split(
        &self,
        env: &str,
        recipients: &[RecipientEntry],
    ) -> Result<Vec<String>, SecretError> {
        if env == ORG_SCOPE || !ikit_core::is_slug(env) {
            return Err(err(format!("`{env}` cannot name a secrets file")));
        }
        if self.has_scope(env) {
            return Err(err(format!(
                "org `{}` already has a {env} secrets file",
                self.org.name
            )));
        }
        let me = self.identity()?.public.clone();
        if !recipients.iter().any(|r| r.key == me) {
            return Err(err("include your own key in the new file's recipients"));
        }
        let org_recipients = self.recipients(ORG_SCOPE)?;
        let mut org = self.with_secrets(ORG_SCOPE, |s| s.clone())?;
        let moved_keys: Vec<String> = org
            .keys()
            .filter(|k| key_env(k).as_deref() == Some(env))
            .cloned()
            .collect();
        let moved: Secrets = moved_keys
            .iter()
            .map(|k| (k.clone(), org.remove(k).expect("listed")))
            .collect();
        // Who may split is who may read the org file; the new file must be readable by them.
        self.encrypt(env, &moved, recipients, &org_recipients)?;
        write_recipients(&recipients_path(&self.org, env), recipients)?;
        // From here on `env` keys resolve to the new file, so the org copy can go.
        self.encrypt(ORG_SCOPE, &org, &org_recipients, &org_recipients)?;
        Ok(moved_keys)
    }

    /// Moves environment `env`'s secrets back into the org file and removes its own file.
    /// Returns the keys moved.
    pub fn merge(&self, env: &str) -> Result<Vec<String>, SecretError> {
        if env == ORG_SCOPE {
            return Err(err("the org file cannot be merged into itself"));
        }
        self.require_scope(env)?;
        let moved = self.with_secrets(env, |s| s.clone())?;
        let org_recipients = self.recipients(ORG_SCOPE)?;
        let mut org = self.with_secrets(ORG_SCOPE, |s| s.clone())?;
        let keys: Vec<String> = moved.keys().cloned().collect();
        org.extend(moved);
        self.encrypt(ORG_SCOPE, &org, &org_recipients, &org_recipients)?;
        for path in [
            secrets_path(&self.org, env),
            recipients_path(&self.org, env),
        ] {
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(|e| err(format!("removing {}: {e}", path.display())))?;
            }
        }
        self.cache.lock().expect("secrets cache").remove(env);
        Ok(keys)
    }

    /// Names of the stored secrets in files this machine can read.
    #[cfg(test)]
    pub fn keys(&self) -> Result<Vec<String>, SecretError> {
        let mut keys = Vec::new();
        for scope in all_recipients(&self.org).into_keys() {
            if let Ok(k) = self.with_secrets(&scope, |s| s.keys().cloned().collect::<Vec<_>>()) {
                keys.extend(k);
            }
        }
        keys.sort();
        Ok(keys)
    }

    fn update(&self, key: &str, value: Option<&str>) -> Result<(), SecretError> {
        let scope = self.scope_for(key);
        let recipients = self.recipients(&scope)?;
        let mut updated = self.with_secrets(&scope, |s| s.clone())?;
        let changed = match value {
            Some(v) => updated
                .insert(key.to_owned(), Zeroizing::new(v.to_owned()))
                .is_none_or(|old| old.as_str() != v),
            None => updated.remove(key).is_some(),
        };
        if changed {
            self.encrypt(&scope, &updated, &recipients, &recipients)?;
        }
        Ok(())
    }
}

impl SecretStore for AgeFileStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        let scope = self.scope_for(key);
        self.with_secrets(&scope, |s| s.get(key).map(|v| v.as_str().to_owned()))
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.update(key, Some(value))
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.update(key, None)
    }
}

#[cfg(test)]
mod tests;
