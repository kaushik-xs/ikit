//! Named profiles from the AWS shared files (`~/.aws/credentials`, `~/.aws/config`), for an
//! ikit `aws` profile that sets `aws_profile` instead of holding keys itself.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{AppError, Result};

/// Static credentials (and region) of one named AWS profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SharedProfile {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: Option<String>,
}

/// Reads profile `name` from the shared files. `AWS_SHARED_CREDENTIALS_FILE` and
/// `AWS_CONFIG_FILE` override the default locations, as in the AWS CLI.
pub(crate) fn load(name: &str) -> Result<SharedProfile> {
    let credentials = shared_file("AWS_SHARED_CREDENTIALS_FILE", "credentials");
    let config = shared_file("AWS_CONFIG_FILE", "config");
    let read = |p: &Option<PathBuf>| {
        p.as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    };
    resolve(name, &read(&credentials), &read(&config))
}

fn shared_file(env: &str, file: &str) -> Option<PathBuf> {
    std::env::var_os(env)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".aws").join(file)))
}

/// Merges the profile's `config` section with its `credentials` section; the credentials
/// file wins, as in the AWS CLI.
fn resolve(name: &str, credentials: &str, config: &str) -> Result<SharedProfile> {
    // In the config file every profile but `default` is written `[profile <name>]`.
    let config_section = if name == "default" {
        "default".to_owned()
    } else {
        format!("profile {name}")
    };
    let from_config = section(config, &config_section);
    let from_credentials = section(credentials, name);
    if from_config.is_none() && from_credentials.is_none() {
        return Err(AppError::Invalid(format!(
            "AWS profile `{name}` not found in ~/.aws/credentials or ~/.aws/config"
        )));
    }
    let mut values = from_config.unwrap_or_default();
    values.extend(from_credentials.unwrap_or_default());

    let (Some(id), Some(secret)) = (
        values.remove("aws_access_key_id"),
        values.remove("aws_secret_access_key"),
    ) else {
        return Err(AppError::Invalid(format!(
            "AWS profile `{name}` has no aws_access_key_id/aws_secret_access_key; \
             SSO, role_arn and credential_process profiles are not supported"
        )));
    };
    Ok(SharedProfile {
        access_key_id: id,
        secret_access_key: secret,
        session_token: values.remove("aws_session_token"),
        region: values.remove("region"),
    })
}

/// The `key = value` pairs of INI section `[name]`, or `None` when it is absent.
fn section(text: &str, name: &str) -> Option<BTreeMap<String, String>> {
    let mut found = None;
    let mut current: Option<&str> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let header = header.trim();
            current = Some(header);
            if header == name {
                found.get_or_insert_with(BTreeMap::new);
            }
            continue;
        }
        if current != Some(name) {
            continue;
        }
        if let (Some(map), Some((k, v))) = (found.as_mut(), line.split_once('=')) {
            map.insert(k.trim().to_lowercase(), v.trim().to_owned());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDENTIALS: &str = "\
[default]
aws_access_key_id = DEFAULTID
aws_secret_access_key = defaultsecret

# a comment
[rg2k]
aws_access_key_id=RGID
aws_secret_access_key = rgsecret
aws_session_token = tok
";

    const CONFIG: &str = "\
[default]
region = us-east-1

[profile rg2k]
region = ap-south-1

[profile sso]
sso_start_url = https://example.awsapps.com/start
region = eu-west-1
";

    #[test]
    fn merges_credentials_and_config() {
        let p = resolve("rg2k", CREDENTIALS, CONFIG).unwrap();
        assert_eq!(
            p,
            SharedProfile {
                access_key_id: "RGID".into(),
                secret_access_key: "rgsecret".into(),
                session_token: Some("tok".into()),
                region: Some("ap-south-1".into()),
            }
        );
        let d = resolve("default", CREDENTIALS, CONFIG).unwrap();
        assert_eq!(d.access_key_id, "DEFAULTID");
        assert_eq!(d.region.as_deref(), Some("us-east-1"));
        assert_eq!(d.session_token, None);
    }

    #[test]
    fn rejects_missing_and_keyless_profiles() {
        let missing = resolve("nope", CREDENTIALS, CONFIG).unwrap_err();
        assert!(missing.to_string().contains("not found"), "{missing}");
        let sso = resolve("sso", CREDENTIALS, CONFIG).unwrap_err();
        assert!(sso.to_string().contains("not supported"), "{sso}");
    }
}
