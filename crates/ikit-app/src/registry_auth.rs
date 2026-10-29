//! Credentials for private image registries (GHCR, ECR, ACR, …), used by `docker login` on
//! the target before an apply pulls images. Passwords and tokens live in the keychain.

use ikit_core::registry::RegistryLogin;

use crate::{App, AppError, Result};

impl App {
    fn registry_secret_key(&self, host: &str) -> String {
        format!("org/{}/registry/{host}", self.org.name)
    }

    pub async fn registry_login(
        &self,
        host: &str,
        username: &str,
        password: &str,
    ) -> Result<RegistryLogin> {
        let valid_host = !host.is_empty()
            && !host.starts_with('-')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
        if !valid_host {
            return Err(AppError::Invalid(format!(
                "invalid registry host `{host}` (e.g. ghcr.io or 123.dkr.ecr.eu-west-1.amazonaws.com)"
            )));
        }
        if username.trim().is_empty() || password.is_empty() {
            return Err(AppError::Invalid(
                "username and password are required".to_owned(),
            ));
        }
        self.secrets
            .set(&self.registry_secret_key(host), password)?;
        Ok(self.store.registry().set_login(host, username).await?)
    }

    pub async fn registry_logins(&self) -> Result<Vec<RegistryLogin>> {
        Ok(self.store.registry().logins().await?)
    }

    pub async fn registry_logout(&self, host: &str) -> Result<()> {
        if !self.store.registry().delete_login(host).await? {
            return Err(AppError::NotFound(format!("registry login for {host}")));
        }
        self.secrets.delete(&self.registry_secret_key(host))?;
        Ok(())
    }

    pub(crate) fn registry_password(&self, host: &str) -> Result<String> {
        self.secrets.get(&self.registry_secret_key(host))?.ok_or_else(|| {
            AppError::Invalid(format!("the keychain has no password for {host}; run `ikit registry login {host}` again"))
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::SecretStore;
    use crate::testing::app;

    #[tokio::test]
    async fn login_stores_password_in_keychain() {
        let (_dir, app, secrets) = app().await;
        let login = app
            .registry_login("ghcr.io", "octocat", "ghp_token")
            .await
            .unwrap();
        assert_eq!(login.username, "octocat");
        assert_eq!(
            secrets.get("org/acme/registry/ghcr.io").unwrap().as_deref(),
            Some("ghp_token")
        );
        assert_eq!(app.registry_logins().await.unwrap().len(), 1);

        assert!(app.registry_login("bad host", "u", "p").await.is_err());
        app.registry_logout("ghcr.io").await.unwrap();
        assert_eq!(secrets.get("org/acme/registry/ghcr.io").unwrap(), None);
        assert!(app.registry_logout("ghcr.io").await.is_err());
    }
}
