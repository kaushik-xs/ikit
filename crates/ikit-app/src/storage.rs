//! Blob storage: where backups (and later state exports) go. One interface over S3 and
//! S3-compatible stores (MinIO), Azure Blob, GCS and a local folder, via `object_store`.

use std::sync::Arc;

use futures::TryStreamExt;
use ikit_core::{ProfileKind, Storage, StorageOptions, StorageProvider};
use ikit_state::NewStorage;
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use serde::Serialize;

use crate::{App, AppError, Result};

#[derive(Debug, Clone)]
pub struct NewStorageRequest {
    pub name: String,
    pub provider: StorageProvider,
    pub bucket: String,
    pub prefix: String,
    pub profile: Option<String>,
    pub options: StorageOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectEntry {
    pub key: String,
    pub size: u64,
}

fn store_error(storage: &str) -> impl Fn(object_store::Error) -> AppError + '_ {
    move |e| AppError::Invalid(format!("storage `{storage}`: {e}"))
}

impl App {
    pub async fn storage_add(&self, req: NewStorageRequest) -> Result<Storage> {
        if !ikit_core::is_slug(&req.name) {
            return Err(AppError::Invalid(format!(
                "invalid storage name `{}`: use {}",
                req.name,
                ikit_core::SLUG_RULE
            )));
        }
        let prefix = req.prefix.trim_matches('/').to_owned();
        let needs_root = req.provider == StorageProvider::Local;
        if needs_root && req.options.root.is_none() {
            return Err(AppError::Invalid(
                "local storage needs --root <folder>".to_owned(),
            ));
        }
        if req.provider == StorageProvider::Azure && req.options.account.is_none() {
            return Err(AppError::Invalid(
                "Azure storage needs --account <storage account>".to_owned(),
            ));
        }
        if req.bucket.trim().is_empty() {
            return Err(AppError::Invalid(
                "a bucket (container for Azure) is required".to_owned(),
            ));
        }
        if let Some(profile) = &req.profile {
            self.profile_show(profile_kind(req.provider)?, profile)
                .await?;
        }
        let new = NewStorage {
            name: &req.name,
            provider: req.provider,
            bucket: &req.bucket,
            prefix: &prefix,
            profile: req.profile.as_deref(),
            options: &req.options,
        };
        self.store
            .storage()
            .insert(new)
            .await
            .map_err(|e| match AppError::from(e) {
                AppError::AlreadyExists(_) => {
                    AppError::AlreadyExists(format!("storage `{}`", req.name))
                }
                e => e,
            })
    }

    pub async fn storage_list(&self) -> Result<Vec<Storage>> {
        Ok(self.store.storage().list().await?)
    }

    pub async fn storage_show(&self, name: &str) -> Result<Storage> {
        self.store
            .storage()
            .get(name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("storage `{name}`")))
    }

    /// The storage `name`, or the only one there is when `name` is `None`.
    pub(crate) async fn storage_or_default(&self, name: Option<&str>) -> Result<Storage> {
        if let Some(n) = name {
            return self.storage_show(n).await;
        }
        let mut all = self.storage_list().await?;
        match all.len() {
            1 => Ok(all.remove(0)),
            0 => Err(AppError::Invalid(
                "no storage yet; add one with `ikit storage add`".to_owned(),
            )),
            _ => Err(AppError::Invalid(
                "several storages exist; pass --storage <name>".to_owned(),
            )),
        }
    }

    /// Removes a storage from ikit. Refused while backups are catalogued in it.
    pub async fn storage_remove(&self, name: &str) -> Result<()> {
        let used: Vec<String> = self
            .store
            .storage()
            .backups(None, None)
            .await?
            .into_iter()
            .filter(|b| b.storage == name)
            .map(|b| format!("backup {}", b.id))
            .collect();
        if !used.is_empty() {
            return Err(AppError::InUse {
                what: format!("storage `{name}`"),
                by: used,
            });
        }
        if !self.store.storage().delete(name).await? {
            return Err(AppError::NotFound(format!("storage `{name}`")));
        }
        Ok(())
    }

    pub(crate) async fn object_store(&self, storage: &Storage) -> Result<Arc<dyn ObjectStore>> {
        let values = match &storage.profile {
            Some(p) => {
                self.profile_values(profile_kind(storage.provider)?, p)
                    .await?
            }
            None => Default::default(),
        };
        let get = |k: &str| values.get(k).cloned();
        let err = store_error(&storage.name);
        let o = &storage.options;
        let store: Arc<dyn ObjectStore> = match storage.provider {
            StorageProvider::S3 => {
                let mut b = object_store::aws::AmazonS3Builder::from_env()
                    .with_bucket_name(&storage.bucket);
                if let Some(region) = o.region.clone().or_else(|| get("region")) {
                    b = b.with_region(region);
                }
                if let (Some(id), Some(secret)) = (get("access_key_id"), get("secret_access_key")) {
                    b = b.with_access_key_id(id).with_secret_access_key(secret);
                }
                if let Some(endpoint) = &o.endpoint {
                    // S3-compatible stores (MinIO) use path-style URLs and often plain HTTP.
                    b = b
                        .with_endpoint(endpoint)
                        .with_virtual_hosted_style_request(false)
                        .with_allow_http(endpoint.starts_with("http://"));
                }
                Arc::new(b.build().map_err(err)?)
            }
            StorageProvider::Azure => {
                let mut b = object_store::azure::MicrosoftAzureBuilder::from_env()
                    .with_container_name(&storage.bucket);
                if let Some(account) = &o.account {
                    b = b.with_account(account);
                }
                if let (Some(tenant), Some(client), Some(secret)) =
                    (get("tenant_id"), get("client_id"), get("client_secret"))
                {
                    b = b
                        .with_tenant_id(tenant)
                        .with_client_id(client)
                        .with_client_secret(secret);
                }
                Arc::new(b.build().map_err(err)?)
            }
            StorageProvider::Gcs => {
                let mut b = object_store::gcp::GoogleCloudStorageBuilder::from_env()
                    .with_bucket_name(&storage.bucket);
                if let Some(path) = get("credentials_file") {
                    b = b.with_service_account_path(path);
                }
                Arc::new(b.build().map_err(err)?)
            }
            StorageProvider::Local => {
                let root = o.root.clone().expect("validated on add");
                std::fs::create_dir_all(&root)?;
                Arc::new(object_store::local::LocalFileSystem::new_with_prefix(&root).map_err(err)?)
            }
        };
        Ok(store)
    }

    pub(crate) async fn put_object(
        &self,
        storage: &Storage,
        key: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let store = self.object_store(storage).await?;
        store
            .put(&ObjectPath::from(key), bytes.into())
            .await
            .map_err(store_error(&storage.name))?;
        Ok(())
    }

    pub(crate) async fn get_object(&self, storage: &Storage, key: &str) -> Result<Vec<u8>> {
        let store = self.object_store(storage).await?;
        let got = store
            .get(&ObjectPath::from(key))
            .await
            .map_err(store_error(&storage.name))?;
        Ok(got
            .bytes()
            .await
            .map_err(store_error(&storage.name))?
            .to_vec())
    }

    pub(crate) async fn delete_object(&self, storage: &Storage, key: &str) -> Result<()> {
        let store = self.object_store(storage).await?;
        match store.delete(&ObjectPath::from(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(store_error(&storage.name)(e)),
        }
    }

    /// Writes, reads back and deletes a small probe object.
    pub async fn storage_test(&self, name: &str) -> Result<String> {
        let storage = self.storage_show(name).await?;
        let key = storage.key(&format!(".ikit-probe-{}", std::process::id()));
        let payload = b"ikit storage probe".to_vec();
        self.put_object(&storage, &key, payload.clone()).await?;
        let back = self.get_object(&storage, &key).await?;
        self.delete_object(&storage, &key).await?;
        if back != payload {
            return Err(AppError::Invalid(format!(
                "storage `{name}` returned different bytes than were written"
            )));
        }
        Ok(format!(
            "wrote, read and deleted {key} in {}",
            storage.describe()
        ))
    }

    /// Objects under the storage's prefix (and `prefix` below it).
    pub async fn storage_ls(&self, name: &str, prefix: Option<&str>) -> Result<Vec<ObjectEntry>> {
        let storage = self.storage_show(name).await?;
        let store = self.object_store(&storage).await?;
        let full = storage.key(prefix.unwrap_or("").trim_matches('/'));
        let path = (!full.is_empty()).then(|| ObjectPath::from(full.trim_end_matches('/')));
        let mut out: Vec<ObjectEntry> = store
            .list(path.as_ref())
            .map_ok(|m| ObjectEntry {
                key: m.location.to_string(),
                size: m.size,
            })
            .try_collect()
            .await
            .map_err(store_error(&storage.name))?;
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

/// The profile kind that holds credentials for a provider.
fn profile_kind(provider: StorageProvider) -> Result<ProfileKind> {
    match provider {
        StorageProvider::S3 => Ok(ProfileKind::Aws),
        StorageProvider::Azure => Ok(ProfileKind::Azure),
        StorageProvider::Gcs => Ok(ProfileKind::Gcp),
        StorageProvider::Local => Err(AppError::Invalid(
            "local storage takes no profile".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::app;

    #[tokio::test]
    async fn local_storage_round_trip() {
        let (dir, app, _) = app().await;
        let root = dir.path().join("blobs");
        let req = NewStorageRequest {
            name: "nas".into(),
            provider: StorageProvider::Local,
            bucket: "local".into(),
            prefix: "/ikit/".into(),
            profile: None,
            options: StorageOptions {
                root: Some(root.clone()),
                ..Default::default()
            },
        };
        let s = app.storage_add(req.clone()).await.unwrap();
        assert_eq!(s.prefix, "ikit");
        assert!(app.storage_add(req).await.is_err(), "duplicate");

        assert!(
            app.storage_test("nas")
                .await
                .unwrap()
                .contains("wrote, read and deleted")
        );
        app.put_object(&s, &s.key("acme/a.bin"), vec![1, 2, 3])
            .await
            .unwrap();
        assert_eq!(
            app.get_object(&s, "ikit/acme/a.bin").await.unwrap(),
            [1, 2, 3]
        );
        let listed = app.storage_ls("nas", None).await.unwrap();
        assert_eq!(
            listed,
            [ObjectEntry {
                key: "ikit/acme/a.bin".into(),
                size: 3
            }]
        );
        assert!(root.join("ikit/acme/a.bin").exists());
        app.delete_object(&s, "ikit/acme/a.bin").await.unwrap();
        app.delete_object(&s, "ikit/acme/a.bin").await.unwrap();
        app.storage_remove("nas").await.unwrap();
    }

    #[tokio::test]
    async fn add_validates_provider_settings() {
        let (_dir, app, _) = app().await;
        let base = NewStorageRequest {
            name: "x".into(),
            provider: StorageProvider::Local,
            bucket: "b".into(),
            prefix: String::new(),
            profile: None,
            options: StorageOptions::default(),
        };
        assert!(
            app.storage_add(base.clone()).await.is_err(),
            "local needs a root"
        );
        let azure = NewStorageRequest {
            provider: StorageProvider::Azure,
            ..base.clone()
        };
        assert!(
            app.storage_add(azure).await.is_err(),
            "azure needs an account"
        );
        let s3 = NewStorageRequest {
            provider: StorageProvider::S3,
            profile: Some("nope".into()),
            ..base
        };
        assert!(matches!(
            app.storage_add(s3).await,
            Err(AppError::ProfileNotFound { .. })
        ));
    }
}
