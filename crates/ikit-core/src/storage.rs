//! Blob storage locations and the backups kept in them.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::hook::SqlEngine;

string_enum! {
    "storage provider",
    pub enum StorageProvider {
        /// AWS S3, or any S3-compatible store (MinIO, Ceph, R2) via `endpoint`.
        S3 => "s3",
        Azure => "azure",
        Gcs => "gcs",
        /// A folder on the machine running ikit (or a mounted share).
        Local => "local",
    }
}

/// Provider-specific settings that are not secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageOptions {
    /// S3: custom endpoint, e.g. `http://localhost:9000` for MinIO.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// S3: region (default: the profile's region, else us-east-1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Azure: storage account name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Local: the folder backups go into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Storage {
    pub id: i64,
    pub name: String,
    pub provider: StorageProvider,
    /// Bucket (S3, GCS), container (Azure), or a label for local storage.
    pub bucket: String,
    /// Key prefix inside the bucket, without leading or trailing `/`.
    pub prefix: String,
    /// Profile with the credentials (aws for S3, azure, gcp). `None` uses the environment.
    pub profile: Option<String>,
    pub options: StorageOptions,
    pub created_at: DateTime<Utc>,
}

impl Storage {
    /// `s3://bucket/prefix`, `local:/path`, …
    pub fn describe(&self) -> String {
        let scheme = match self.provider {
            StorageProvider::S3 => "s3",
            StorageProvider::Azure => "az",
            StorageProvider::Gcs => "gs",
            StorageProvider::Local => {
                let root = self
                    .options
                    .root
                    .as_ref()
                    .map(|r| r.display().to_string())
                    .unwrap_or_default();
                return format!("local:{root}");
            }
        };
        if self.prefix.is_empty() {
            format!("{scheme}://{}", self.bucket)
        } else {
            format!("{scheme}://{}/{}", self.bucket, self.prefix)
        }
    }

    /// The full object key for `key` under this storage's prefix.
    pub fn key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Backup {
    pub id: i64,
    pub workload: String,
    pub service: String,
    pub engine: SqlEngine,
    pub storage: String,
    /// Object key inside the storage (including its prefix).
    pub object_key: String,
    /// Compressed size in bytes.
    pub size: i64,
    /// Of the stored (compressed) bytes.
    pub sha256: String,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage(provider: StorageProvider, prefix: &str) -> Storage {
        Storage {
            id: 1,
            name: "s".into(),
            provider,
            bucket: "backups".into(),
            prefix: prefix.into(),
            profile: None,
            options: StorageOptions::default(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn describes_and_prefixes_keys() {
        let s = storage(StorageProvider::S3, "ikit");
        assert_eq!(s.describe(), "s3://backups/ikit");
        assert_eq!(s.key("a/b.zst"), "ikit/a/b.zst");
        assert_eq!(storage(StorageProvider::Gcs, "").key("x"), "x");
    }
}
