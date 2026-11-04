//! Bearer tokens for `ikit mcp serve --http`, in `orgs/<org>/mcp-tokens.yaml` (owner-only).
//! Only SHA-256 hashes are stored; a token is shown once, when it is created. A token can be
//! limited to some workloads; within them, the org's `mcp.yaml` still decides what it may do.

use std::fs;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::deploy::write_private;
use crate::{App, AppError, OrgPaths, Result};

const PREFIX: &str = "ikit_";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToken {
    pub name: String,
    /// Hex SHA-256 of the token.
    pub sha256: String,
    /// Workloads the token can reach; empty means every workload in the org.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workloads: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenFile {
    #[serde(default)]
    tokens: Vec<McpToken>,
}

fn hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn load(org: &OrgPaths) -> Result<TokenFile> {
    let path = org.mcp_tokens_file();
    match fs::read_to_string(&path) {
        Ok(yaml) => {
            serde_yaml_ng::from_str(&yaml).map_err(|source| AppError::Yaml { path, source })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TokenFile::default()),
        Err(e) => Err(e.into()),
    }
}

fn save(org: &OrgPaths, file: &TokenFile) -> Result<()> {
    let yaml = serde_yaml_ng::to_string(file).expect("token file serializes");
    Ok(write_private(&org.mcp_tokens_file(), &yaml)?)
}

/// The token matching `presented`, if any. The file is read on every call so a removed
/// token stops working without restarting the server.
pub fn mcp_token_verify(org: &OrgPaths, presented: &str) -> Result<Option<McpToken>> {
    if !presented.starts_with(PREFIX) {
        return Ok(None);
    }
    let digest = hash(presented);
    Ok(load(org)?.tokens.into_iter().find(|t| t.sha256 == digest))
}

impl App {
    /// Creates a token and returns it with its record. The token itself is not stored.
    /// `workloads` limits it to those workloads, which must exist; empty means the whole org.
    pub async fn mcp_token_create(
        &self,
        name: &str,
        workloads: &[String],
    ) -> Result<(McpToken, String)> {
        if !ikit_core::is_slug(name) {
            return Err(AppError::Invalid(format!(
                "invalid token name `{name}`: use {}",
                ikit_core::SLUG_RULE
            )));
        }
        let mut scope = Vec::new();
        for w in workloads {
            self.workload_get(w).await?;
            if !scope.contains(w) {
                scope.push(w.clone());
            }
        }
        scope.sort();
        let mut file = load(&self.org)?;
        if file.tokens.iter().any(|t| t.name == name) {
            return Err(AppError::AlreadyExists(format!("MCP token `{name}`")));
        }
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|e| AppError::Invalid(format!("no secure random source: {e}")))?;
        let token = format!(
            "{PREFIX}{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let record = McpToken {
            name: name.to_owned(),
            sha256: hash(&token),
            workloads: scope,
            created_at: Utc::now(),
        };
        file.tokens.push(record.clone());
        save(&self.org, &file)?;
        Ok((record, token))
    }

    pub fn mcp_token_list(&self) -> Result<Vec<McpToken>> {
        Ok(load(&self.org)?.tokens)
    }

    pub fn mcp_token_remove(&self, name: &str) -> Result<()> {
        let mut file = load(&self.org)?;
        let before = file.tokens.len();
        file.tokens.retain(|t| t.name != name);
        if file.tokens.len() == before {
            return Err(AppError::NotFound(format!("MCP token `{name}`")));
        }
        save(&self.org, &file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::app;

    #[tokio::test]
    async fn tokens_verify_until_removed() {
        let (_dir, app, _) = app().await;
        let (record, token) = app.mcp_token_create("laptop", &[]).await.unwrap();
        assert!(token.starts_with("ikit_") && token.len() == 69);
        assert_ne!(record.sha256, token);
        assert!(
            !fs::read_to_string(app.org().mcp_tokens_file())
                .unwrap()
                .contains(&token)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(app.org().mcp_tokens_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        assert_eq!(
            mcp_token_verify(app.org(), &token).unwrap().map(|t| t.name),
            Some("laptop".to_owned())
        );
        assert_eq!(mcp_token_verify(app.org(), "ikit_nope").unwrap(), None);
        assert!(
            app.mcp_token_create("laptop", &[]).await.is_err(),
            "names are unique"
        );

        app.mcp_token_remove("laptop").unwrap();
        assert_eq!(mcp_token_verify(app.org(), &token).unwrap(), None);
        assert!(app.mcp_token_remove("laptop").is_err());
    }

    #[tokio::test]
    async fn tokens_can_be_limited_to_existing_workloads() {
        let (_dir, app, _) = app().await;
        app.target_attach("laptop", ikit_core::Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        app.workload_create("dev", "laptop").await.unwrap();

        let e = app
            .mcp_token_create("ci", &["wl-acme-dev-009".into()])
            .await
            .unwrap_err();
        assert!(e.to_string().contains("wl-acme-dev-009"), "{e}");
        assert!(app.mcp_token_list().unwrap().is_empty());

        let wls = [
            "wl-acme-dev-002".into(),
            "wl-acme-dev-001".into(),
            "wl-acme-dev-002".into(),
        ];
        let (record, token) = app.mcp_token_create("ci", &wls).await.unwrap();
        assert_eq!(record.workloads, ["wl-acme-dev-001", "wl-acme-dev-002"]);
        let found = mcp_token_verify(app.org(), &token).unwrap().unwrap();
        assert_eq!(found.workloads, record.workloads);

        // Org-wide tokens keep the old file format.
        app.mcp_token_create("all", &[]).await.unwrap();
        let yaml = fs::read_to_string(app.org().mcp_tokens_file()).unwrap();
        assert_eq!(yaml.matches("workloads:").count(), 1, "{yaml}");
    }
}
