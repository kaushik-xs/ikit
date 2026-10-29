//! Provider profiles: named connection settings for a cloud, cluster or network API.
//!
//! Non-secret fields are stored in the state database; secret fields go to the OS keychain.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    Aws,
    Azure,
    Gcp,
    Zerotier,
    K8s,
}

impl ProfileKind {
    pub const ALL: [ProfileKind; 5] =
        [Self::Aws, Self::Azure, Self::Gcp, Self::Zerotier, Self::K8s];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aws => "aws",
            Self::Azure => "azure",
            Self::Gcp => "gcp",
            Self::Zerotier => "zerotier",
            Self::K8s => "k8s",
        }
    }

    /// Fields accepted by this kind, in prompt order. A `RequiredWith` field always
    /// comes after the field it depends on.
    pub fn fields(self) -> &'static [FieldSpec] {
        match self {
            Self::Aws => AWS_FIELDS,
            Self::Azure => AZURE_FIELDS,
            Self::Gcp => GCP_FIELDS,
            Self::Zerotier => ZEROTIER_FIELDS,
            Self::K8s => K8S_FIELDS,
        }
    }

    pub fn field(self, key: &str) -> Option<&'static FieldSpec> {
        self.fields().iter().find(|f| f.key == key)
    }
}

impl fmt::Display for ProfileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProfileKind {
    type Err = ProfileError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| ProfileError::UnknownKind(s.to_owned()))
    }
}

use Requirement::{Optional, Required, RequiredWith};

const AWS_FIELDS: &[FieldSpec] = &[
    FieldSpec::new("region", "Default AWS region, e.g. eu-west-1", Required),
    FieldSpec::new("aws_profile", "Named profile from ~/.aws/config", Optional),
    FieldSpec::new(
        "access_key_id",
        "Static access key id (omit to use the default credential chain)",
        Optional,
    ),
    FieldSpec::new(
        "secret_access_key",
        "Secret access key",
        RequiredWith("access_key_id"),
    )
    .secret(),
];

const AZURE_FIELDS: &[FieldSpec] = &[
    FieldSpec::new("tenant_id", "Azure AD tenant id", Required),
    FieldSpec::new("subscription_id", "Subscription id", Required),
    FieldSpec::new(
        "client_id",
        "Service principal client id (omit to use az login)",
        Optional,
    ),
    FieldSpec::new(
        "client_secret",
        "Service principal secret",
        RequiredWith("client_id"),
    )
    .secret(),
];

const GCP_FIELDS: &[FieldSpec] = &[
    FieldSpec::new("project_id", "GCP project id", Required),
    FieldSpec::new("region", "Default region, e.g. europe-west1", Optional),
    FieldSpec::new(
        "credentials_file",
        "Service account JSON path (omit to use ADC)",
        Optional,
    ),
];

const ZEROTIER_FIELDS: &[FieldSpec] = &[
    FieldSpec::new("api_url", "ZeroTier Central API URL", Optional)
        .default_value("https://api.zerotier.com/api/v1"),
    FieldSpec::new("api_token", "ZeroTier Central API token", Required).secret(),
];

const K8S_FIELDS: &[FieldSpec] = &[
    FieldSpec::new("context", "kubeconfig context name", Required),
    FieldSpec::new(
        "kubeconfig",
        "kubeconfig path (default: $KUBECONFIG or ~/.kube/config)",
        Optional,
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    Required,
    Optional,
    /// Required when the named field has a value.
    RequiredWith(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    pub key: &'static str,
    pub description: &'static str,
    pub requirement: Requirement,
    pub secret: bool,
    pub default: Option<&'static str>,
}

impl FieldSpec {
    const fn new(key: &'static str, description: &'static str, requirement: Requirement) -> Self {
        Self {
            key,
            description,
            requirement,
            secret: false,
            default: None,
        }
    }

    const fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    const fn default_value(mut self, value: &'static str) -> Self {
        self.default = Some(value);
        self
    }

    /// Whether this field must have a value, given the values collected so far.
    pub fn is_required(&self, values: &BTreeMap<String, String>) -> bool {
        match self.requirement {
            Requirement::Required => true,
            Requirement::Optional => false,
            Requirement::RequiredWith(other) => values.contains_key(other),
        }
    }
}

/// Validated profile values, split by where they are stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileValues {
    pub config: BTreeMap<String, String>,
    pub secrets: BTreeMap<String, String>,
}

/// Checks `name` and `values` against the kind's field specs, applies defaults and
/// splits secrets from plain config. Empty values count as missing.
pub fn validate(
    kind: ProfileKind,
    name: &str,
    values: BTreeMap<String, String>,
) -> Result<ProfileValues, ProfileError> {
    validate_name(name)?;

    let mut values: BTreeMap<String, String> = values
        .into_iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .collect();

    if let Some(key) = values.keys().find(|k| kind.field(k).is_none()) {
        return Err(ProfileError::UnknownField {
            kind,
            key: key.clone(),
        });
    }

    for field in kind.fields() {
        if let Some(default) = field.default {
            values
                .entry(field.key.to_owned())
                .or_insert_with(|| default.to_owned());
        }
    }

    for field in kind.fields() {
        if field.is_required(&values) && !values.contains_key(field.key) {
            return Err(ProfileError::MissingField {
                kind,
                key: field.key.to_owned(),
            });
        }
    }

    let (secrets, config) = values
        .into_iter()
        .partition(|(k, _)| kind.field(k).is_some_and(|f| f.secret));
    Ok(ProfileValues { config, secrets })
}

/// Profile names are lowercase slugs so they work as CLI args and keychain accounts.
pub fn validate_name(name: &str) -> Result<(), ProfileError> {
    if crate::is_slug(name) {
        Ok(())
    } else {
        Err(ProfileError::InvalidName(name.to_owned()))
    }
}

/// A stored profile. Secret values are never part of this type, only their keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Profile {
    pub id: i64,
    pub kind: ProfileKind,
    pub name: String,
    pub config: BTreeMap<String, String>,
    pub secret_keys: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("unknown profile kind `{0}` (expected one of: aws, azure, gcp, zerotier, k8s)")]
    UnknownKind(String),
    #[error(
        "invalid profile name `{0}`: use 1-63 lowercase letters, digits or `-`, starting with a letter or digit"
    )]
    InvalidName(String),
    #[error("`{key}` is not a field of {kind} profiles (see `ikit profile fields {kind}`)")]
    UnknownField { kind: ProfileKind, key: String },
    #[error("{kind} profiles require `{key}`")]
    MissingField { kind: ProfileKind, key: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn kind_round_trips_through_str() {
        for kind in ProfileKind::ALL {
            assert_eq!(kind.as_str().parse::<ProfileKind>(), Ok(kind));
        }
        assert!("digitalocean".parse::<ProfileKind>().is_err());
    }

    #[test]
    fn splits_secrets_from_config() {
        let v = validate(
            ProfileKind::Aws,
            "acme",
            values(&[
                ("region", "eu-west-1"),
                ("access_key_id", "AKIA"),
                ("secret_access_key", "s3cr3t"),
            ]),
        )
        .unwrap();
        assert_eq!(
            v.config,
            values(&[("region", "eu-west-1"), ("access_key_id", "AKIA")])
        );
        assert_eq!(v.secrets, values(&[("secret_access_key", "s3cr3t")]));
    }

    #[test]
    fn required_with_only_applies_when_dependency_set() {
        assert!(validate(ProfileKind::Aws, "acme", values(&[("region", "eu-west-1")])).is_ok());
        assert_eq!(
            validate(
                ProfileKind::Aws,
                "acme",
                values(&[("region", "eu-west-1"), ("access_key_id", "AKIA")])
            ),
            Err(ProfileError::MissingField {
                kind: ProfileKind::Aws,
                key: "secret_access_key".into()
            })
        );
    }

    #[test]
    fn applies_defaults() {
        let v = validate(ProfileKind::Zerotier, "zt", values(&[("api_token", "t")])).unwrap();
        assert_eq!(v.config["api_url"], "https://api.zerotier.com/api/v1");
    }

    #[test]
    fn empty_values_count_as_missing() {
        assert_eq!(
            validate(ProfileKind::K8s, "kind", values(&[("context", "  ")])),
            Err(ProfileError::MissingField {
                kind: ProfileKind::K8s,
                key: "context".into()
            })
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(matches!(
            validate(
                ProfileKind::K8s,
                "kind",
                values(&[("context", "x"), ("region", "y")])
            ),
            Err(ProfileError::UnknownField { .. })
        ));
    }

    #[test]
    fn validates_names() {
        for ok in ["acme", "acme-prod", "1a"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-acme", "Acme", "acme_prod", "a/b", &"a".repeat(64)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }
}
