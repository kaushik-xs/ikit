//! The service registry: deployable services, their versions and what each version needs.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::version::{Dependency, VersionConstraint, parse_version};

string_enum! {
    "category",
    /// Which kind of service group a registry service belongs to.
    pub enum Category {
        App => "app",
        Platform => "platform",
        Data => "data",
    }
}

string_enum! {
    "registry source",
    /// Where a registry service came from.
    pub enum Source {
        Seed => "seed",
        Manual => "manual",
        Import => "import",
    }
}

/// What a registry version needs to run. Stored as one JSON document per version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<Port>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<Dependency>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Port {
    pub container: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "is_tcp")]
    pub protocol: Protocol,
}

fn is_tcp(p: &Protocol) -> bool {
    *p == Protocol::Tcp
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvVar {
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Allowed values, if restricted.
    #[serde(default, rename = "enum", skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
    /// Filled from another service's output at deploy time, e.g. `service:postgres.url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl EnvVar {
    /// Whether a value has to be supplied when the service is added to a workload.
    pub fn needs_value(&self) -> bool {
        self.required && self.default.is_none() && self.from.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Healthcheck {
    /// Command run inside the container, e.g. `["pg_isready", "-U", "postgres"]`.
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
}

impl ServiceSpec {
    pub fn env_var(&self, name: &str) -> Option<&EnvVar> {
        self.env.iter().find(|e| e.name == name)
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut names = BTreeSet::new();
        for e in &self.env {
            if e.name.is_empty()
                || !e
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Err(format!("invalid env var name `{}`", e.name));
            }
            if !names.insert(&e.name) {
                return Err(format!("env var `{}` is declared twice", e.name));
            }
            if e.secret && e.default.is_some() {
                return Err(format!("secret env var `{}` cannot have a default", e.name));
            }
            if let Some(d) = &e.default
                && !e.allowed.is_empty()
                && !e.allowed.contains(d)
            {
                return Err(format!(
                    "default of `{}` is not one of its allowed values",
                    e.name
                ));
            }
        }
        let mut volumes = BTreeSet::new();
        for v in &self.volumes {
            if !crate::is_slug(&v.name) || !volumes.insert(&v.name) {
                return Err(format!("invalid or duplicate volume name `{}`", v.name));
            }
            if !v.path.starts_with('/') {
                return Err(format!("volume `{}` path must be absolute", v.name));
            }
        }
        Ok(())
    }
}

/// A registry service as stored, without its versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistryService {
    pub id: i64,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub category: Category,
    pub tags: Vec<String>,
    pub owner: Option<String>,
    pub homepage: Option<String>,
    pub source: Source,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistryVersion {
    pub id: i64,
    pub version: Version,
    pub image: String,
    pub digest: Option<String>,
    pub spec: ServiceSpec,
    pub release_notes: Option<String>,
    pub deprecated: bool,
    pub created_at: DateTime<Utc>,
}

/// A service with all its versions, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistryEntry {
    #[serde(flatten)]
    pub service: RegistryService,
    pub versions: Vec<RegistryVersion>,
}

impl RegistryEntry {
    pub fn resolve(&self, constraint: &VersionConstraint) -> Option<&RegistryVersion> {
        let best = constraint.resolve(self.versions.iter().map(|v| (&v.version, v.deprecated)))?;
        self.versions.iter().find(|v| &v.version == best)
    }

    pub fn version(&self, version: &Version) -> Option<&RegistryVersion> {
        self.versions.iter().find(|v| &v.version == version)
    }

    pub fn latest(&self) -> Option<&RegistryVersion> {
        self.resolve(&VersionConstraint::Latest)
    }
}

/// `registry.yaml`: the import/export format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryFile {
    pub services: Vec<ServiceDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub category: Category,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    pub versions: Vec<VersionDef>,
}

/// One version in `registry.yaml`. The spec fields sit inline next to `version` and
/// `image` (not flattened, so unknown keys are still rejected).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionDef {
    #[serde(with = "version_str")]
    pub version: Version,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deprecated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_notes: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<Port>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<Dependency>,
}

impl VersionDef {
    pub fn new(version: Version, image: String, spec: ServiceSpec) -> Self {
        let ServiceSpec {
            ports,
            env,
            volumes,
            healthcheck,
            resources,
            depends_on,
        } = spec;
        Self {
            version,
            image,
            digest: None,
            deprecated: false,
            release_notes: None,
            ports,
            env,
            volumes,
            healthcheck,
            resources,
            depends_on,
        }
    }

    pub fn spec(&self) -> ServiceSpec {
        ServiceSpec {
            ports: self.ports.clone(),
            env: self.env.clone(),
            volumes: self.volumes.clone(),
            healthcheck: self.healthcheck.clone(),
            resources: self.resources.clone(),
            depends_on: self.depends_on.clone(),
        }
    }
}

/// Versions in `registry.yaml` accept short forms (`16`, `"16.4"`) and are written out
/// normalized (`16.4.0`).
mod version_str {
    use semver::Version;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Version, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Version, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Scalar {
            S(String),
            I(u64),
            F(f64),
        }
        let raw = match Scalar::deserialize(d)? {
            Scalar::S(s) => s,
            Scalar::I(i) => i.to_string(),
            // YAML reads `16.10` as the float 16.1, so a float can't be trusted.
            Scalar::F(f) => {
                return Err(serde::de::Error::custom(format!(
                    "version {f} must be quoted, e.g. version: \"{f}\""
                )));
            }
        };
        super::parse_version(&raw).map_err(serde::de::Error::custom)
    }
}

impl ServiceDef {
    /// Checks names, versions and specs; `context` prefixes error messages.
    pub fn validate(&self) -> Result<(), RegistryError> {
        let err = |msg: String| RegistryError::Invalid {
            service: self.name.clone(),
            message: msg,
        };
        if !crate::is_slug(&self.name) {
            return Err(err(format!("invalid name: use {}", crate::SLUG_RULE)));
        }
        if self.versions.is_empty() {
            return Err(err("at least one version is required".into()));
        }
        let mut seen = BTreeSet::new();
        for v in &self.versions {
            if !seen.insert(&v.version) {
                return Err(err(format!("version {} is listed twice", v.version)));
            }
            if v.image.trim().is_empty() || v.image.contains(char::is_whitespace) {
                return Err(err(format!(
                    "version {}: invalid image `{}`",
                    v.version, v.image
                )));
            }
            v.spec()
                .validate()
                .map_err(|m| err(format!("version {}: {m}", v.version)))?;
            if v.depends_on.iter().any(|d| d.name == self.name) {
                return Err(err(format!("version {} depends on itself", v.version)));
            }
        }
        Ok(())
    }
}

impl From<&RegistryEntry> for ServiceDef {
    fn from(e: &RegistryEntry) -> Self {
        let s = &e.service;
        Self {
            name: s.name.clone(),
            display_name: s.display_name.clone(),
            description: s.description.clone(),
            category: s.category,
            tags: s.tags.clone(),
            owner: s.owner.clone(),
            homepage: s.homepage.clone(),
            versions: e
                .versions
                .iter()
                .map(|v| VersionDef {
                    digest: v.digest.clone(),
                    deprecated: v.deprecated,
                    release_notes: v.release_notes.clone(),
                    ..VersionDef::new(v.version.clone(), v.image.clone(), v.spec.clone())
                })
                .collect(),
        }
    }
}

/// Whether an image reference uses a moving tag (`:latest` or no tag at all, and no digest).
pub fn is_moving_tag(image: &str) -> bool {
    if image.contains('@') {
        return false;
    }
    let last = image.rsplit('/').next().unwrap_or(image);
    match last.split_once(':') {
        None => true,
        Some((_, tag)) => tag == "latest",
    }
}

string_enum! {
    "import mode",
    pub enum ImportMode {
        /// Add new services and versions, update existing ones.
        Merge => "merge",
        /// Like merge, then delete services and versions missing from the file (unless in use).
        Replace => "replace",
        /// Only add what is missing; never touch existing entries. Used for built-in seeds.
        InsertMissing => "insert-missing",
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub services_added: Vec<String>,
    pub services_updated: Vec<String>,
    pub services_removed: Vec<String>,
    /// `name@version` entries.
    pub versions_added: Vec<String>,
    pub versions_updated: Vec<String>,
    pub versions_removed: Vec<String>,
    /// Entries `--replace` would have removed but which workloads still use.
    pub kept_in_use: Vec<String>,
}

/// Credentials for pulling from an image registry. The password is in the keychain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistryLogin {
    pub host: String,
    pub username: String,
    pub created_at: DateTime<Utc>,
}

/// A workload service using a registry service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub workload: String,
    pub service: String,
    pub version: Version,
}

/// Keys of `env` that are unknown to `spec`, as an error message.
pub fn unknown_env(spec: &ServiceSpec, env: &BTreeMap<String, String>) -> Option<String> {
    let unknown: Vec<&str> = env
        .keys()
        .filter(|k| spec.env_var(k).is_none())
        .map(String::as_str)
        .collect();
    (!unknown.is_empty()).then(|| unknown.join(", "))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("registry service `{service}`: {message}")]
    Invalid { service: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = r#"
services:
  - name: postgres
    category: data
    tags: [sql]
    versions:
      - version: "16.4"
        image: postgres:16.4
        ports: [{container: 5432, name: postgres}]
        env:
          - {name: POSTGRES_PASSWORD, required: true, secret: true}
          - {name: POSTGRES_DB, default: app}
        volumes: [{name: data, path: /var/lib/postgresql/data}]
      - version: 17
        image: postgres:17
  - name: orders-api
    category: app
    versions:
      - version: 1.4.2
        image: ghcr.io/acme/orders-api:1.4.2
        depends_on: ["postgres@^16"]
"#;

    #[test]
    fn parses_and_round_trips_registry_yaml() {
        let file: RegistryFile = serde_yaml_ng::from_str(YAML).unwrap();
        assert_eq!(file.services.len(), 2);
        let pg = &file.services[0];
        assert_eq!(pg.versions[0].version, Version::new(16, 4, 0));
        assert_eq!(pg.versions[1].version, Version::new(17, 0, 0));
        assert!(
            pg.versions[0]
                .spec()
                .env_var("POSTGRES_PASSWORD")
                .unwrap()
                .needs_value()
        );
        for s in &file.services {
            s.validate().unwrap();
        }

        let out = serde_yaml_ng::to_string(&file).unwrap();
        let again: RegistryFile = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(again, file);
    }

    #[test]
    fn rejects_unknown_fields_and_bad_specs() {
        let bad = YAML.replace("tags: [sql]", "tagz: [sql]");
        assert!(serde_yaml_ng::from_str::<RegistryFile>(&bad).is_err());

        let mut file: RegistryFile = serde_yaml_ng::from_str(YAML).unwrap();
        file.services[0].versions[0].env[0].default = Some("x".into());
        assert!(file.services[0].validate().is_err(), "secret with default");

        let float = YAML.replace(r#"version: "16.4""#, "version: 16.10");
        let err = serde_yaml_ng::from_str::<RegistryFile>(&float)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be quoted"), "{err}");
    }

    #[test]
    fn detects_moving_tags() {
        assert!(is_moving_tag("nginx"));
        assert!(is_moving_tag("nginx:latest"));
        assert!(is_moving_tag("localhost:5000/nginx"));
        assert!(!is_moving_tag("localhost:5000/nginx:1.27"));
        assert!(!is_moving_tag("nginx@sha256:abc"));
    }
}
