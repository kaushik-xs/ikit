use std::collections::BTreeMap;

use ikit_core::{Profile, ProfileKind, profile};
use ikit_state::{NewProfile, StateError};
use serde::Deserialize;

use crate::{App, AppError, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct AddProfile {
    pub kind: ProfileKind,
    pub name: String,
    /// All field values, secret and non-secret; they are split by the kind's field specs.
    pub values: BTreeMap<String, String>,
}

impl App {
    pub async fn profile_add(&self, req: AddProfile) -> Result<Profile> {
        let AddProfile { kind, name, values } = req;
        let values = profile::validate(kind, &name, values)?;
        let secret_keys: Vec<String> = values.secrets.keys().cloned().collect();

        // Insert the row first so a name conflict never touches another profile's secret.
        let new = NewProfile {
            kind,
            name: &name,
            config: &values.config,
            secret_keys: &secret_keys,
        };
        let created = self
            .store
            .profiles()
            .insert(new)
            .await
            .map_err(|e| match e {
                StateError::Conflict(_) => AppError::ProfileExists {
                    kind,
                    name: name.clone(),
                },
                e => e.into(),
            })?;

        if !values.secrets.is_empty() {
            let json = serde_json::to_string(&values.secrets).expect("string map serializes");
            if let Err(e) = self
                .secrets
                .set(&self.profile_secret_key(kind, &name), &json)
            {
                if let Err(undo) = self.store.profiles().delete(kind, &name).await {
                    tracing::error!(%undo, "failed to remove profile after keychain error");
                }
                return Err(e.into());
            }
        }
        Ok(created)
    }

    /// Sets and unsets fields of an existing profile. Secret fields go to the keychain; the
    /// result must still be a valid profile of its kind.
    pub async fn profile_update(
        &self,
        kind: ProfileKind,
        name: &str,
        set: BTreeMap<String, String>,
        unset: &[String],
    ) -> Result<Profile> {
        let existing = self.profile_show(kind, name).await?;
        let key = self.profile_secret_key(kind, name);
        let mut values = existing.config.clone();
        if !existing.secret_keys.is_empty()
            && let Some(json) = self.secrets.get(&key)?
        {
            let secrets: BTreeMap<String, String> = serde_json::from_str(&json).map_err(|e| {
                AppError::Invalid(format!("keychain entry {key} is not valid JSON: {e}"))
            })?;
            values.extend(secrets);
        }
        for k in unset {
            if values.remove(k).is_none() {
                return Err(AppError::Invalid(format!(
                    "{kind}/{name} has no value for `{k}`"
                )));
            }
        }
        values.extend(set);
        let values = profile::validate(kind, name, values)?;
        let secret_keys: Vec<String> = values.secrets.keys().cloned().collect();

        if values.secrets.is_empty() {
            self.secrets.delete(&key)?;
        } else {
            let json = serde_json::to_string(&values.secrets).expect("string map serializes");
            self.secrets.set(&key, &json)?;
        }
        let new = NewProfile {
            kind,
            name,
            config: &values.config,
            secret_keys: &secret_keys,
        };
        self.store
            .profiles()
            .update(new)
            .await?
            .ok_or_else(|| AppError::ProfileNotFound {
                kind,
                name: name.to_owned(),
            })
    }

    /// A profile's config and secret values together, for building clients.
    pub(crate) async fn profile_values(
        &self,
        kind: ProfileKind,
        name: &str,
    ) -> Result<BTreeMap<String, String>> {
        let profile = self.profile_show(kind, name).await?;
        let mut values = profile.config.clone();
        if !profile.secret_keys.is_empty() {
            let key = self.profile_secret_key(kind, name);
            let json = self.secrets.get(&key)?.ok_or_else(|| {
                AppError::Invalid(format!(
                    "keychain entry {key} is missing; set it again with `ikit profile set`"
                ))
            })?;
            let secrets: BTreeMap<String, String> = serde_json::from_str(&json).map_err(|e| {
                AppError::Invalid(format!("keychain entry {key} is not valid JSON: {e}"))
            })?;
            values.extend(secrets);
        }
        Ok(values)
    }

    pub async fn profile_list(&self, kind: Option<ProfileKind>) -> Result<Vec<Profile>> {
        Ok(self.store.profiles().list(kind).await?)
    }

    pub async fn profile_show(&self, kind: ProfileKind, name: &str) -> Result<Profile> {
        self.store
            .profiles()
            .get(kind, name)
            .await?
            .ok_or_else(|| AppError::ProfileNotFound {
                kind,
                name: name.to_owned(),
            })
    }

    pub async fn profile_remove(&self, kind: ProfileKind, name: &str) -> Result<()> {
        let profile = self.profile_show(kind, name).await?;
        self.store.profiles().delete(kind, name).await?;
        if !profile.secret_keys.is_empty() {
            self.secrets.delete(&self.profile_secret_key(kind, name))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretStore;
    use crate::testing::app;

    fn add(kind: ProfileKind, name: &str, pairs: &[(&str, &str)]) -> AddProfile {
        AddProfile {
            kind,
            name: name.to_owned(),
            values: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[tokio::test]
    async fn add_stores_secrets_in_keychain_only() {
        let (_dir, app, secrets) = app().await;
        let profile = app
            .profile_add(add(ProfileKind::Zerotier, "home", &[("api_token", "tok")]))
            .await
            .unwrap();

        assert_eq!(profile.secret_keys, ["api_token"]);
        assert!(!profile.config.contains_key("api_token"));
        assert_eq!(
            secrets
                .get("org/acme/profile/zerotier/home")
                .unwrap()
                .as_deref(),
            Some(r#"{"api_token":"tok"}"#)
        );
        assert_eq!(
            app.profile_show(ProfileKind::Zerotier, "home")
                .await
                .unwrap(),
            profile
        );
    }

    #[tokio::test]
    async fn duplicate_add_keeps_existing_secret() {
        let (_dir, app, secrets) = app().await;
        app.profile_add(add(
            ProfileKind::Zerotier,
            "home",
            &[("api_token", "first")],
        ))
        .await
        .unwrap();

        let err = app
            .profile_add(add(
                ProfileKind::Zerotier,
                "home",
                &[("api_token", "second")],
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::ProfileExists { .. }), "{err}");
        assert_eq!(
            secrets
                .get("org/acme/profile/zerotier/home")
                .unwrap()
                .as_deref(),
            Some(r#"{"api_token":"first"}"#)
        );
    }

    #[tokio::test]
    async fn update_merges_config_and_secrets() {
        let (_dir, app, secrets) = app().await;
        app.profile_add(add(ProfileKind::Aws, "acme", &[("region", "eu-west-1")]))
            .await
            .unwrap();
        let set = BTreeMap::from([
            ("access_key_id".to_owned(), "AKIA".to_owned()),
            ("secret_access_key".to_owned(), "s3cr3t".to_owned()),
        ]);
        let p = app
            .profile_update(ProfileKind::Aws, "acme", set, &[])
            .await
            .unwrap();
        assert_eq!(p.config["region"], "eu-west-1");
        assert_eq!(p.secret_keys, ["secret_access_key"]);
        assert!(
            secrets
                .get("org/acme/profile/aws/acme")
                .unwrap()
                .unwrap()
                .contains("s3cr3t")
        );

        // The result must still be valid: region is required.
        let err = app
            .profile_update(
                ProfileKind::Aws,
                "acme",
                BTreeMap::new(),
                &["region".into()],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Profile(_)), "{err}");
        let p = app
            .profile_update(
                ProfileKind::Aws,
                "acme",
                BTreeMap::new(),
                &["access_key_id".into(), "secret_access_key".into()],
            )
            .await
            .unwrap();
        assert!(p.secret_keys.is_empty());
        assert_eq!(secrets.get("org/acme/profile/aws/acme").unwrap(), None);
    }

    #[tokio::test]
    async fn remove_deletes_row_and_secret() {
        let (_dir, app, secrets) = app().await;
        app.profile_add(add(ProfileKind::Zerotier, "home", &[("api_token", "tok")]))
            .await
            .unwrap();
        app.profile_add(add(ProfileKind::K8s, "kind", &[("context", "kind-kind")]))
            .await
            .unwrap();

        app.profile_remove(ProfileKind::Zerotier, "home")
            .await
            .unwrap();
        assert_eq!(secrets.get("org/acme/profile/zerotier/home").unwrap(), None);
        assert_eq!(app.profile_list(None).await.unwrap().len(), 1);
        assert!(matches!(
            app.profile_remove(ProfileKind::Zerotier, "home").await,
            Err(AppError::ProfileNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn invalid_values_are_rejected_before_anything_is_written() {
        let (_dir, app, _) = app().await;
        let err = app
            .profile_add(add(ProfileKind::Aws, "acme", &[]))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Profile(_)), "{err}");
        assert!(app.profile_list(None).await.unwrap().is_empty());
    }
}
