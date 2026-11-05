//! Remote state: an org's shared copy in object storage, as numbered snapshots with a
//! `HEAD.json` naming the current one and a `lock.json` held while someone writes.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::storage::StorageProvider;

/// Where an org's remote state lives, kept in the local, unsynced `remote.yaml`. It is
/// self-contained: reaching the remote must not need the state it holds, so credentials
/// come from `aws_profile`, a credentials file or the environment, never the keychain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSpec {
    pub provider: StorageProvider,
    /// Bucket (S3, GCS), container (Azure), or a label for local storage.
    pub bucket: String,
    /// Key prefix of this org's state, without leading or trailing `/`.
    pub prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Azure storage account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Local storage: the folder holding the bucket's contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    /// S3: named profile in ~/.aws/credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
    /// GCS: service account key file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_file: Option<PathBuf>,
}

impl RemoteSpec {
    /// `s3://bucket/prefix`, `local:/root/prefix`, …
    pub fn describe(&self) -> String {
        let scheme = match self.provider {
            StorageProvider::S3 => "s3",
            StorageProvider::Azure => "az",
            StorageProvider::Gcs => "gs",
            StorageProvider::Local => {
                let root = self
                    .root
                    .as_ref()
                    .map(|r| r.display().to_string())
                    .unwrap_or_default();
                return format!("local:{}/{}", root.trim_end_matches('/'), self.prefix);
            }
        };
        format!("{scheme}://{}/{}", self.bucket, self.prefix)
    }

    /// Parses `s3://bucket/prefix`, `az://container/prefix`, `gs://bucket/prefix` or
    /// `local:/folder/prefix` (the last path segment is the prefix). Other fields are unset.
    pub fn parse_url(url: &str) -> Result<Self, String> {
        let bad = || {
            format!(
                "invalid remote `{url}`: use s3://bucket/prefix, az://container/prefix, \
                 gs://bucket/prefix or local:/folder/prefix"
            )
        };
        if let Some(path) = url.strip_prefix("local:") {
            let path = PathBuf::from(path.trim_end_matches('/'));
            let prefix = path
                .file_name()
                .ok_or_else(bad)?
                .to_string_lossy()
                .into_owned();
            let root = path.parent().ok_or_else(bad)?.to_path_buf();
            return Ok(Self::new(StorageProvider::Local, "local", &prefix).with_root(root));
        }
        let (scheme, rest) = url.split_once("://").ok_or_else(bad)?;
        let provider = match scheme {
            "s3" => StorageProvider::S3,
            "az" => StorageProvider::Azure,
            "gs" => StorageProvider::Gcs,
            _ => return Err(bad()),
        };
        let (bucket, prefix) = rest.split_once('/').ok_or_else(bad)?;
        let prefix = prefix.trim_matches('/');
        if bucket.is_empty() || prefix.is_empty() {
            return Err(bad());
        }
        Ok(Self::new(provider, bucket, prefix))
    }

    pub fn new(provider: StorageProvider, bucket: &str, prefix: &str) -> Self {
        Self {
            provider,
            bucket: bucket.to_owned(),
            prefix: prefix.trim_matches('/').to_owned(),
            region: None,
            endpoint: None,
            account: None,
            root: None,
            aws_profile: None,
            credentials_file: None,
        }
    }

    fn with_root(mut self, root: PathBuf) -> Self {
        self.root = Some(root);
        self
    }
}

/// `HEAD.json`: the current generation of an org's remote state. Replaced only by a
/// conditional write, so a writer working from an older HEAD cannot overwrite it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub format: u32,
    pub org: String,
    pub generation: u64,
    /// Key of the snapshot bundle, relative to the org's prefix.
    pub snapshot: String,
    /// SHA-256 of the snapshot object as stored (compressed).
    pub sha256: String,
    /// SHA-256 of the bundle's contents, used to tell whether local state has changed.
    pub content_sha256: String,
    /// Newest state migration in the snapshot; a newer one needs a newer ikit.
    pub schema_version: i64,
    pub ikit_version: String,
    pub written_by: String,
    pub written_at: DateTime<Utc>,
    pub command: String,
}

impl Head {
    pub const FORMAT: u32 = 1;

    /// `snapshots/000042-<id>.tar.zst`. The random `id` keeps a snapshot left by a writer
    /// that crashed before updating HEAD from blocking that generation number.
    pub fn snapshot_key(generation: u64, id: &str) -> String {
        format!("snapshots/{generation:06}-{id}.tar.zst")
    }

    /// The generation a snapshot key or file name belongs to (`000042.tar.zst` from before
    /// snapshots had ids, or `000042-<id>.tar.zst`).
    pub fn snapshot_generation(key: &str) -> Option<u64> {
        let name = key.rsplit('/').next()?.strip_suffix(".tar.zst")?;
        let digits = name.split_once('-').map_or(name, |(g, _)| g);
        (digits.len() == 6).then(|| digits.parse().ok()).flatten()
    }

    /// `history/000042.json`: a copy of HEAD as it was when that generation was written.
    pub fn history_key(generation: u64) -> String {
        format!("history/{generation:06}.json")
    }
}

/// `lock.json`: present while someone writes. A lease: renewed while the command runs,
/// so a crashed writer's lock lapses at `expires_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    /// Random id of this holding, so a writer releases only its own lock.
    pub id: String,
    pub holder: String,
    pub host: String,
    pub pid: u32,
    pub command: String,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl LockInfo {
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_describes_urls() {
        let s3 = RemoteSpec::parse_url("s3://regere-ikit-backups/ikit/state/regere/").unwrap();
        assert_eq!(s3.provider, StorageProvider::S3);
        assert_eq!(s3.bucket, "regere-ikit-backups");
        assert_eq!(s3.prefix, "ikit/state/regere");
        assert_eq!(s3.describe(), "s3://regere-ikit-backups/ikit/state/regere");

        let local = RemoteSpec::parse_url("local:/mnt/nas/state/acme").unwrap();
        assert_eq!(local.root, Some(PathBuf::from("/mnt/nas/state")));
        assert_eq!(local.prefix, "acme");
        assert_eq!(local.describe(), "local:/mnt/nas/state/acme");

        for bad in ["s3://bucket", "s3:///x", "ftp://b/p", "regere", "local:/"] {
            assert!(RemoteSpec::parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn snapshot_keys_sort_by_generation() {
        assert_eq!(Head::snapshot_key(42, "ab"), "snapshots/000042-ab.tar.zst");
        assert!(Head::snapshot_key(9, "ff") < Head::snapshot_key(10, "00"));
        assert_eq!(Head::snapshot_generation("x/snapshots/000042-ab.tar.zst"), Some(42));
        assert_eq!(Head::snapshot_generation("snapshots/000007.tar.zst"), Some(7));
        assert_eq!(Head::snapshot_generation("snapshots/notes.txt"), None);
        assert_eq!(Head::snapshot_generation("snapshots/42.tar.zst"), None);
        assert_eq!(Head::history_key(3), "history/000003.json");
    }
}
