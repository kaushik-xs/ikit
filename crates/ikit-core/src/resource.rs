//! Resources a service needs besides its container (a bucket, a queue), the backends that
//! provide them, and what ikit has created or adopted for each workload.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

string_enum! {
    "resource kind",
    pub enum ResourceKind {
        Bucket => "bucket",
        Queue => "queue",
    }
}

string_enum! {
    "resource API",
    /// The wire protocol a service's code speaks to a resource. Any backend that speaks it
    /// can provide the resource.
    pub enum ResourceApi {
        /// S3 and S3-compatible stores (MinIO, RustFS, Garage, R2).
        S3 => "s3",
        /// SQS and SQS-compatible queues (ElasticMQ).
        Sqs => "sqs",
    }
}

string_enum! {
    "resource provider",
    /// How a backend creates resources.
    pub enum ResourceProvider {
        /// The AWS APIs, or a compatible server at the backend's `endpoint`.
        Aws => "aws",
    }
}

string_enum! {
    "resource origin",
    pub enum ResourceOrigin {
        /// ikit created it, and may delete it.
        Created => "created",
        /// It existed before; ikit only forgets it.
        Adopted => "adopted",
    }
}

string_enum! {
    "resource status",
    pub enum ResourceStatus {
        /// Recorded; created on the next apply.
        Pending => "pending",
        /// Exists at the backend.
        Ready => "ready",
    }
}

impl ResourceApi {
    /// The kind of resource this API provides.
    pub fn kind(self) -> ResourceKind {
        match self {
            ResourceApi::S3 => ResourceKind::Bucket,
            ResourceApi::Sqs => ResourceKind::Queue,
        }
    }

    /// Fields a `resource:<name>.<field>` reference can read, and whether each is secret.
    pub fn fields(self) -> &'static [(&'static str, bool)] {
        match self {
            ResourceApi::S3 => &[
                ("name", false),
                ("arn", false),
                ("url", false),
                ("region", false),
                ("endpoint", false),
                ("access_key_id", true),
                ("secret_access_key", true),
            ],
            ResourceApi::Sqs => &[
                ("name", false),
                ("url", false),
                ("arn", false),
                ("dlq_url", false),
                ("region", false),
                ("endpoint", false),
                ("access_key_id", true),
                ("secret_access_key", true),
            ],
        }
    }

    /// Whether `field` is one of [`fields`](Self::fields), and if so whether it is secret.
    pub fn field(self, field: &str) -> Option<bool> {
        self.fields()
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, secret)| *secret)
    }
}

/// A resource a registry version needs: its own (`api`), or another service's (`use`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Need {
    /// Local to the service, e.g. `assets`; env vars refer to it as `resource:assets.<field>`.
    pub name: String,
    /// Optional, implied by `api`; checked against it when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ResourceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ResourceApi>,
    /// `<service>.<resource>`: share a resource another service in the workload declares.
    #[serde(default, rename = "use", skip_serializing_if = "Option::is_none")]
    pub uses: Option<String>,
    #[serde(default, skip_serializing_if = "NeedOptions::is_empty")]
    pub options: NeedOptions,
}

/// Settings ikit applies when it creates the resource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeedOptions {
    /// Queue: seconds a received message stays hidden from other consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility_timeout: Option<u32>,
    /// Queue: seconds a message is kept (SQS: 60 to 1209600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<u32>,
    /// Queue: move messages received this many times to `<queue>-dlq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_letter: Option<DeadLetter>,
    /// Bucket: keep old versions of overwritten and deleted objects.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub versioning: bool,
}

impl NeedOptions {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeadLetter {
    pub max_receive: u32,
}

impl Need {
    /// The service and resource a `use` need points at.
    pub fn used(&self) -> Option<(&str, &str)> {
        self.uses.as_deref().and_then(|u| u.split_once('.'))
    }

    pub fn validate(&self) -> Result<(), String> {
        if !crate::is_slug(&self.name) {
            return Err(format!(
                "invalid resource name `{}`: use {}",
                self.name,
                crate::SLUG_RULE
            ));
        }
        match (&self.uses, self.api) {
            (Some(u), None) => {
                let ok = u
                    .split_once('.')
                    .is_some_and(|(s, r)| crate::is_slug(s) && crate::is_slug(r));
                if !ok {
                    return Err(format!(
                        "resource `{}`: `use` must be <service>.<resource>, not `{u}`",
                        self.name
                    ));
                }
                if self.kind.is_some() || !self.options.is_empty() {
                    return Err(format!(
                        "resource `{}` uses `{u}`; its kind and options come from there",
                        self.name
                    ));
                }
            }
            (None, Some(api)) => {
                if let Some(kind) = self.kind
                    && kind != api.kind()
                {
                    return Err(format!(
                        "resource `{}`: api {api} provides a {}, not a {kind}",
                        self.name,
                        api.kind()
                    ));
                }
                let o = &self.options;
                let queue_only = o.visibility_timeout.is_some()
                    || o.retention.is_some()
                    || o.dead_letter.is_some();
                match api.kind() {
                    ResourceKind::Bucket if queue_only => {
                        return Err(format!(
                            "resource `{}` is a bucket; visibility_timeout, retention and dead_letter are queue options",
                            self.name
                        ));
                    }
                    ResourceKind::Queue if o.versioning => {
                        return Err(format!(
                            "resource `{}` is a queue; versioning is a bucket option",
                            self.name
                        ));
                    }
                    _ => {}
                }
                if o.dead_letter.as_ref().is_some_and(|d| d.max_receive == 0) {
                    return Err(format!(
                        "resource `{}`: dead_letter.max_receive must be at least 1",
                        self.name
                    ));
                }
            }
            (Some(_), Some(_)) => {
                return Err(format!(
                    "resource `{}` has both `api` and `use`; pick one",
                    self.name
                ));
            }
            (None, None) => {
                return Err(format!(
                    "resource `{}` needs an `api` (s3, sqs) or `use: <service>.<resource>`",
                    self.name
                ));
            }
        }
        Ok(())
    }
}

/// Parses `resource:<name>.<field>`; `None` for references to something else.
pub fn parse_reference(reference: &str) -> Option<Result<(&str, &str), String>> {
    let rest = reference.strip_prefix("resource:")?;
    Some(
        rest.split_once('.')
            .filter(|(n, f)| !n.is_empty() && !f.is_empty())
            .ok_or_else(|| format!("reference `{reference}` must be resource:<name>.<field>")),
    )
}

/// Settings of a backend that are not secrets. Credentials come from its profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendOptions {
    /// Default: the profile's region, else us-east-1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// A compatible server instead of AWS, e.g. `http://minio.lan:9000`. Services get it as
    /// the `endpoint` field, so it must be reachable from the target too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// An org-level place that provides one API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Backend {
    pub id: i64,
    pub name: String,
    pub api: ResourceApi,
    pub provider: ResourceProvider,
    /// An `aws` profile with credentials; `None` uses the environment.
    pub profile: Option<String>,
    pub options: BackendOptions,
    pub created_at: DateTime<Utc>,
}

/// Which backend provides an API for workloads in an env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Binding {
    pub env: String,
    pub api: ResourceApi,
    pub backend: String,
}

/// A resource of one service in a workload, as recorded in the state database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resource {
    pub id: i64,
    pub workload: String,
    pub service: String,
    pub name: String,
    pub api: ResourceApi,
    /// Set by `ikit res bind`, or by the first apply from the env's binding.
    pub backend: Option<String>,
    /// Bucket or queue name at the backend.
    pub external_id: String,
    /// Non-secret fields known once it exists (url, arn, dlq_url, …).
    pub outputs: BTreeMap<String, String>,
    pub origin: ResourceOrigin,
    pub status: ResourceStatus,
    /// Its service was removed; kept until `ikit res rm`.
    pub orphaned: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Resource {
    pub fn kind(&self) -> ResourceKind {
        self.api.kind()
    }

    /// `<service>.<name>`, as the CLI names it.
    pub fn address(&self) -> String {
        format!("{}.{}", self.service, self.name)
    }
}

/// The default name of a resource at its backend: `<workload>-<service>-<resource>`, cut to
/// the API's length limit (S3 63, SQS 80) and, for buckets, made unique with `suffix`,
/// because bucket names are global.
pub fn default_external_id(
    api: ResourceApi,
    workload: &str,
    service: &str,
    resource: &str,
    suffix: &str,
) -> String {
    let base = format!("{workload}-{service}-{resource}");
    let (max, tail) = match api {
        // A queue's dead-letter queue is `<name>-dlq`.
        ResourceApi::Sqs => (80 - "-dlq".len(), String::new()),
        ResourceApi::S3 => (63, format!("-{suffix}")),
    };
    let room = max - tail.len();
    let mut cut: String = base.chars().take(room).collect();
    while cut.ends_with('-') {
        cut.pop();
    }
    format!("{cut}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn need(yaml: &str) -> Need {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn needs_validate() {
        assert!(need("{name: jobs, api: sqs}").validate().is_ok());
        assert!(
            need("{name: jobs, kind: queue, api: sqs, options: {dead_letter: {max_receive: 5}}}")
                .validate()
                .is_ok()
        );
        assert!(
            need("{name: jobs, use: simulation-service.jobs}")
                .validate()
                .is_ok()
        );
        for bad in [
            "{name: jobs}",
            "{name: Jobs, api: sqs}",
            "{name: jobs, kind: bucket, api: sqs}",
            "{name: jobs, api: sqs, use: a.b}",
            "{name: jobs, use: nodot}",
            "{name: jobs, use: a.b, kind: queue}",
            "{name: assets, api: s3, options: {visibility_timeout: 30}}",
            "{name: jobs, api: sqs, options: {versioning: true}}",
            "{name: jobs, api: sqs, options: {dead_letter: {max_receive: 0}}}",
        ] {
            assert!(need(bad).validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn references_parse() {
        assert_eq!(
            parse_reference("resource:jobs.url"),
            Some(Ok(("jobs", "url")))
        );
        assert!(parse_reference("resource:jobs").unwrap().is_err());
        assert!(parse_reference("service:postgres.host").is_none());
        assert_eq!(ResourceApi::Sqs.field("dlq_url"), Some(false));
        assert_eq!(ResourceApi::S3.field("secret_access_key"), Some(true));
        assert_eq!(ResourceApi::S3.field("dlq_url"), None);
    }

    #[test]
    fn external_ids_fit_the_api() {
        assert_eq!(
            default_external_id(
                ResourceApi::Sqs,
                "wl-acme-dev-002",
                "simulation-service",
                "jobs",
                "x"
            ),
            "wl-acme-dev-002-simulation-service-jobs"
        );
        assert_eq!(
            default_external_id(
                ResourceApi::S3,
                "wl-acme-dev-002",
                "simulation-service",
                "assets",
                "7f3k2a"
            ),
            "wl-acme-dev-002-simulation-service-assets-7f3k2a"
        );
        let long = "a".repeat(70);
        let id = default_external_id(
            ResourceApi::S3,
            "wl-acme-dev-002",
            &long,
            "assets",
            "7f3k2a",
        );
        assert_eq!(id.len(), 63);
        assert!(id.ends_with("-7f3k2a"));
        let id = default_external_id(ResourceApi::Sqs, "wl-acme-dev-002", &long, "jobs", "");
        assert!(id.len() <= 76 && !id.ends_with('-'));
    }
}
