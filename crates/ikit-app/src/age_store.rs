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

/// The secrets file every org has; per-environment files come later.
pub const SCOPE: &str = "org";
/// Keychain account holding this machine's private key.
pub const IDENTITY_KEY: &str = "identity/default";
/// Environment variable holding a private key (`AGE-SECRET-KEY-…`), e.g. in CI.
pub const IDENTITY_ENV: &str = "IKIT_AGE_IDENTITY";

pub fn recipients_file(org: &OrgPaths) -> PathBuf {
    org.dir.join("recipients").join(format!("{SCOPE}.txt"))
}

pub fn secrets_file(org: &OrgPaths) -> PathBuf {
    org.dir.join("secrets").join(format!("{SCOPE}.age"))
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
            Some((k, l)) => (k.trim(), Some(l.trim().to_owned()).filter(|l| !l.is_empty())),
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
    let mut text = String::from("# Public keys that can read secrets/org.age (ikit org secrets grant|revoke)\n");
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
        let parsed = age::ssh::Identity::from_buffer(
            text.as_bytes(),
            Some(path.display().to_string()),
        )
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
                return Err(err(format!("{}: unsupported SSH key: {k:?}", path.display())));
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

/// Secrets in `secrets/org.age`, decrypted on first use and kept in memory only.
pub struct AgeFileStore {
    org: OrgPaths,
    identity: Option<Arc<LocalIdentity>>,
    cache: Mutex<Option<Secrets>>,
}

impl AgeFileStore {
    pub fn new(org: OrgPaths, identity: Option<Arc<LocalIdentity>>) -> Self {
        Self {
            org,
            identity,
            cache: Mutex::new(None),
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

    pub fn recipients(&self) -> Result<Vec<RecipientEntry>, SecretError> {
        read_recipients(&recipients_file(&self.org))
    }

    fn with_secrets<T>(&self, f: impl FnOnce(&mut Secrets) -> T) -> Result<T, SecretError> {
        let mut cache = self.cache.lock().expect("secrets cache");
        if cache.is_none() {
            *cache = Some(self.decrypt()?);
        }
        Ok(f(cache.as_mut().expect("loaded")))
    }

    fn decrypt(&self) -> Result<Secrets, SecretError> {
        let path = secrets_file(&self.org);
        if !path.exists() {
            return Ok(Secrets::new());
        }
        let identity = self.identity()?;
        let bytes =
            std::fs::read(&path).map_err(|e| err(format!("reading {}: {e}", path.display())))?;
        let decryptor = age::Decryptor::new(bytes.as_slice())
            .map_err(|e| err(format!("{}: {e}", path.display())))?;
        let mut reader = decryptor
            .decrypt(std::iter::once(identity.identity.as_ref() as &dyn age::Identity))
            .map_err(|e| match e {
                age::DecryptError::NoMatchingKeys => err(format!(
                    "your key {} cannot read {}; ask a teammate to run \
                     `ikit org secrets grant {}`",
                    identity.public,
                    path.display(),
                    identity.public
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

    /// Encrypts `secrets` to `recipients`. Refused unless this machine's key is in `writers`
    /// (the recipients before the change): only someone who can already read the secrets
    /// may change them or who can read them.
    fn encrypt(
        &self,
        secrets: &Secrets,
        recipients: &[RecipientEntry],
        writers: &[RecipientEntry],
    ) -> Result<(), SecretError> {
        let me = &self.identity()?.public;
        if !writers.iter().any(|r| &r.key == me) {
            return Err(err(format!(
                "your key {me} is not a recipient of org `{}`'s secrets, so you cannot change them",
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
        write_file(&secrets_file(&self.org), &out)
    }

    /// Starts the encrypted file with `recipients` (which must include this machine's key)
    /// and `secrets`.
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
        self.encrypt(&secrets, recipients, recipients)?;
        write_recipients(&recipients_file(&self.org), recipients)?;
        *self.cache.lock().expect("secrets cache") = Some(secrets);
        Ok(())
    }

    /// Adds a recipient and re-encrypts. Returns false when the key was already listed.
    pub fn grant(&self, entry: RecipientEntry) -> Result<bool, SecretError> {
        let mut recipients = self.recipients()?;
        if recipients.iter().any(|r| r.key == entry.key) {
            return Ok(false);
        }
        recipients.push(entry);
        self.rewrite(&recipients)?;
        Ok(true)
    }

    /// Removes the recipient whose key or label is `who` and re-encrypts.
    pub fn revoke(&self, who: &str) -> Result<RecipientEntry, SecretError> {
        let mut recipients = self.recipients()?;
        let who = who.trim();
        let i = recipients
            .iter()
            .position(|r| r.key == who || r.label.as_deref() == Some(who))
            .ok_or_else(|| err(format!("`{who}` is not a recipient")))?;
        if recipients.len() == 1 {
            return Err(err(
                "that is the last recipient; nobody could read the secrets after revoking it",
            ));
        }
        let removed = recipients.remove(i);
        self.rewrite(&recipients)?;
        Ok(removed)
    }

    /// Re-encrypts to a changed recipient list. The secrets are decrypted first, so this
    /// works even when the change removes this machine's own key.
    fn rewrite(&self, recipients: &[RecipientEntry]) -> Result<(), SecretError> {
        let before = self.recipients()?;
        let secrets = self.with_secrets(|s| s.clone())?;
        self.encrypt(&secrets, recipients, &before)?;
        write_recipients(&recipients_file(&self.org), recipients)
    }

    /// Names of the stored secrets.
    pub fn keys(&self) -> Result<Vec<String>, SecretError> {
        self.with_secrets(|s| s.keys().cloned().collect())
    }
}

impl SecretStore for AgeFileStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        self.with_secrets(|s| s.get(key).map(|v| v.as_str().to_owned()))
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        let recipients = self.recipients()?;
        let mut updated = self.with_secrets(|s| s.clone())?;
        updated.insert(key.to_owned(), Zeroizing::new(value.to_owned()));
        self.encrypt(&updated, &recipients, &recipients)?;
        *self.cache.lock().expect("secrets cache") = Some(updated);
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let recipients = self.recipients()?;
        let mut updated = self.with_secrets(|s| s.clone())?;
        if updated.remove(key).is_none() {
            return Ok(());
        }
        self.encrypt(&updated, &recipients, &recipients)?;
        *self.cache.lock().expect("secrets cache") = Some(updated);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
