//! S3 buckets and SQS queues on AWS, or on a compatible server at the backend's endpoint
//! (MinIO, RustFS, Garage for S3; ElasticMQ for SQS).

use std::collections::BTreeMap;

use async_trait::async_trait;
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata};
use aws_sdk_s3::types::{
    BucketLocationConstraint, BucketVersioningStatus, CreateBucketConfiguration, Tag, Tagging,
    VersioningConfiguration,
};
use aws_sdk_sqs::types::QueueAttributeName;
use ikit_core::ResourceApi;

use super::{Access, IamUser, Keys, ResourceDriver, Want};
use crate::{AppError, Result};

/// The longest SQS keeps messages, used for dead-letter queues so failures can be looked at.
const DLQ_RETENTION: &str = "1209600";

/// How long a new access key gets to start working; IAM changes take seconds to spread.
const KEY_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) struct AwsDriver {
    backend: String,
    region: String,
    conf: aws_config::SdkConfig,
    s3: aws_sdk_s3::Client,
    sqs: aws_sdk_sqs::Client,
    iam: aws_sdk_iam::Client,
    sts: aws_sdk_sts::Client,
}

impl AwsDriver {
    pub(super) async fn new(backend: &str, access: &Access) -> Self {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(access.region.clone()));
        if let Some(k) = &access.keys {
            loader = loader.credentials_provider(aws_sdk_s3::config::Credentials::new(
                k.access_key_id.clone(),
                k.secret_access_key.clone(),
                k.session_token.clone(),
                None,
                "ikit",
            ));
        }
        let conf = loader.load().await;
        let mut s3 = aws_sdk_s3::config::Builder::from(&conf);
        let mut sqs = aws_sdk_sqs::config::Builder::from(&conf);
        if let Some(endpoint) = &access.endpoint {
            // Compatible servers serve buckets by path, not as subdomains.
            s3 = s3.endpoint_url(endpoint).force_path_style(true);
            sqs = sqs.endpoint_url(endpoint);
        }
        Self {
            backend: backend.to_owned(),
            region: access.region.clone(),
            s3: aws_sdk_s3::Client::from_conf(s3.build()),
            sqs: aws_sdk_sqs::Client::from_conf(sqs.build()),
            iam: aws_sdk_iam::Client::new(&conf),
            sts: aws_sdk_sts::Client::new(&conf),
            conf,
        }
    }

    /// AWS's own code and message when it sent them, else the whole error chain.
    fn error(&self, what: &str, e: impl std::error::Error + ProvideErrorMetadata) -> AppError {
        let detail = match (e.code(), e.message()) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            _ => DisplayErrorContext(e).to_string(),
        };
        AppError::Invalid(format!("backend {}: {what}: {detail}", self.backend))
    }

    // ---- S3 ----

    async fn ensure_bucket(&self, want: &Want<'_>) -> Result<BTreeMap<String, String>> {
        let name = want.name;
        let mut create = self.s3.create_bucket().bucket(name);
        // us-east-1 is the default and rejects being named.
        if self.region != "us-east-1" {
            create = create.create_bucket_configuration(
                CreateBucketConfiguration::builder()
                    .location_constraint(BucketLocationConstraint::from(self.region.as_str()))
                    .build(),
            );
        }
        if let Err(e) = create.send().await {
            match e.as_service_error() {
                Some(s) if s.is_bucket_already_owned_by_you() => {}
                Some(s) if s.is_bucket_already_exists() => {
                    return Err(AppError::Invalid(format!(
                        "backend {}: bucket name `{name}` is taken by another account; bucket names are global, \
                         so remove the resource's service and add it again for a new name",
                        self.backend
                    )));
                }
                _ => return Err(self.error(&format!("creating bucket {name}"), e)),
            }
        }
        let tags: Vec<Tag> = want
            .tags
            .iter()
            .filter_map(|(k, v)| Tag::builder().key(k).value(v).build().ok())
            .collect();
        let tagging = Tagging::builder()
            .set_tag_set(Some(tags))
            .build()
            .expect("tag set is set");
        // Some compatible servers don't support tags; the bucket is usable without them.
        if let Err(e) = self
            .s3
            .put_bucket_tagging()
            .bucket(name)
            .tagging(tagging)
            .send()
            .await
        {
            tracing::warn!(
                "backend {}: tagging bucket {name}: {}",
                self.backend,
                DisplayErrorContext(e)
            );
        }
        if want.options.versioning {
            self.s3
                .put_bucket_versioning()
                .bucket(name)
                .versioning_configuration(
                    VersioningConfiguration::builder()
                        .status(BucketVersioningStatus::Enabled)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| self.error(&format!("turning on versioning for {name}"), e))?;
        }
        Ok(BTreeMap::new())
    }

    async fn bucket_exists(&self, name: &str) -> Result<bool> {
        match self.s3.head_bucket().bucket(name).send().await {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().is_some_and(|s| s.is_not_found()) => Ok(false),
            Err(e) => Err(self.error(&format!("looking up bucket {name}"), e)),
        }
    }

    // ---- SQS ----

    async fn queue_url(&self, name: &str) -> Result<Option<String>> {
        match self.sqs.get_queue_url().queue_name(name).send().await {
            Ok(out) => Ok(out.queue_url().map(str::to_owned)),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_queue_does_not_exist()) =>
            {
                Ok(None)
            }
            Err(e) => Err(self.error(&format!("looking up queue {name}"), e)),
        }
    }

    async fn queue_attributes(&self, url: &str) -> Result<BTreeMap<QueueAttributeName, String>> {
        let out = self
            .sqs
            .get_queue_attributes()
            .queue_url(url)
            .attribute_names(QueueAttributeName::QueueArn)
            .attribute_names(QueueAttributeName::RedrivePolicy)
            .send()
            .await
            .map_err(|e| self.error(&format!("reading queue {url}"), e))?;
        Ok(out
            .attributes()
            .map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }

    /// Creates queue `name` with `attributes` and `tags`, or sets them on the existing one.
    /// Returns its URL and ARN.
    async fn put_queue(
        &self,
        name: &str,
        attributes: &BTreeMap<QueueAttributeName, String>,
        tags: &BTreeMap<String, String>,
    ) -> Result<(String, String)> {
        let mut create = self.sqs.create_queue().queue_name(name);
        for (k, v) in attributes {
            create = create.attributes(k.clone(), v);
        }
        for (k, v) in tags {
            create = create.tags(k, v);
        }
        let url = match create.send().await {
            Ok(out) => out.queue_url().unwrap_or_default().to_owned(),
            // It exists with other attributes: bring them in line.
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_queue_name_exists()) =>
            {
                let url = self.queue_url(name).await?.ok_or_else(|| {
                    AppError::Invalid(format!("backend {}: queue {name} vanished", self.backend))
                })?;
                if !attributes.is_empty() {
                    let mut set = self.sqs.set_queue_attributes().queue_url(&url);
                    for (k, v) in attributes {
                        set = set.attributes(k.clone(), v);
                    }
                    set.send()
                        .await
                        .map_err(|e| self.error(&format!("updating queue {name}"), e))?;
                }
                url
            }
            Err(e) => return Err(self.error(&format!("creating queue {name}"), e)),
        };
        let arn = self
            .queue_attributes(&url)
            .await?
            .remove(&QueueAttributeName::QueueArn)
            .unwrap_or_default();
        Ok((url, arn))
    }

    async fn ensure_queue(&self, want: &Want<'_>) -> Result<BTreeMap<String, String>> {
        let o = want.options;
        let mut outputs = BTreeMap::new();
        let mut attributes = BTreeMap::new();
        if let Some(v) = o.visibility_timeout {
            attributes.insert(QueueAttributeName::VisibilityTimeout, v.to_string());
        }
        if let Some(v) = o.retention {
            attributes.insert(QueueAttributeName::MessageRetentionPeriod, v.to_string());
        }
        if let Some(dl) = &o.dead_letter {
            let dlq = format!("{}-dlq", want.name);
            let dlq_attributes = BTreeMap::from([(
                QueueAttributeName::MessageRetentionPeriod,
                DLQ_RETENTION.to_owned(),
            )]);
            let (dlq_url, dlq_arn) = self.put_queue(&dlq, &dlq_attributes, want.tags).await?;
            let policy = serde_json::json!({
                "deadLetterTargetArn": dlq_arn,
                "maxReceiveCount": dl.max_receive.to_string(),
            });
            attributes.insert(QueueAttributeName::RedrivePolicy, policy.to_string());
            outputs.insert("dlq_url".to_owned(), dlq_url);
        }
        let (url, arn) = self.put_queue(want.name, &attributes, want.tags).await?;
        outputs.insert("url".to_owned(), url);
        outputs.insert("arn".to_owned(), arn);
        Ok(outputs)
    }

    /// A queue by name or URL: its name, URL, ARN and dead-letter queue URL.
    async fn inspect_queue(&self, id: &str) -> Result<Option<(String, BTreeMap<String, String>)>> {
        let url = if id.starts_with("http://") || id.starts_with("https://") {
            Some(id.to_owned())
        } else {
            self.queue_url(id).await?
        };
        let Some(url) = url else { return Ok(None) };
        let name = url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(id)
            .to_owned();
        let mut attrs = self.queue_attributes(&url).await?;
        let mut outputs = BTreeMap::from([
            ("url".to_owned(), url),
            (
                "arn".to_owned(),
                attrs
                    .remove(&QueueAttributeName::QueueArn)
                    .unwrap_or_default(),
            ),
        ]);
        let dlq = attrs
            .remove(&QueueAttributeName::RedrivePolicy)
            .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
            .and_then(|p| p["deadLetterTargetArn"].as_str().map(str::to_owned))
            .and_then(|arn| arn.rsplit(':').next().map(str::to_owned));
        if let Some(dlq) = dlq
            && let Some(dlq_url) = self.queue_url(&dlq).await?
        {
            outputs.insert("dlq_url".to_owned(), dlq_url);
        }
        Ok(Some((name, outputs)))
    }
}

#[async_trait]
impl ResourceDriver for AwsDriver {
    async fn ensure(&self, want: &Want<'_>) -> Result<BTreeMap<String, String>> {
        match want.api {
            ResourceApi::S3 => self.ensure_bucket(want).await,
            ResourceApi::Sqs => self.ensure_queue(want).await,
        }
    }

    async fn inspect(
        &self,
        api: ResourceApi,
        id: &str,
    ) -> Result<Option<(String, BTreeMap<String, String>)>> {
        match api {
            ResourceApi::S3 => Ok(self
                .bucket_exists(id)
                .await?
                .then(|| (id.to_owned(), BTreeMap::new()))),
            ResourceApi::Sqs => self.inspect_queue(id).await,
        }
    }

    async fn delete(
        &self,
        api: ResourceApi,
        name: &str,
        outputs: &BTreeMap<String, String>,
    ) -> Result<()> {
        match api {
            ResourceApi::S3 => {
                if let Err(e) = self.s3.delete_bucket().bucket(name).send().await {
                    if e.as_service_error()
                        .is_some_and(|s| s.meta().code() == Some("BucketNotEmpty"))
                    {
                        return Err(AppError::Invalid(format!(
                            "backend {}: bucket {name} is not empty; empty it first (e.g. `aws s3 rm s3://{name} --recursive`), \
                             or keep it with --keep",
                            self.backend
                        )));
                    }
                    return Err(self.error(&format!("deleting bucket {name}"), e));
                }
            }
            ResourceApi::Sqs => {
                for key in ["url", "dlq_url"] {
                    let Some(url) = outputs.get(key) else {
                        continue;
                    };
                    if let Err(e) = self.sqs.delete_queue().queue_url(url).send().await
                        && !e
                            .as_service_error()
                            .is_some_and(|s| s.is_queue_does_not_exist())
                    {
                        return Err(self.error(&format!("deleting queue {url}"), e));
                    }
                }
            }
        }
        Ok(())
    }

    async fn whoami(&self) -> Result<(String, String)> {
        let out = self
            .sts
            .get_caller_identity()
            .send()
            .await
            .map_err(|e| self.error("looking up the provisioning identity", e))?;
        Ok((
            out.account().unwrap_or_default().to_owned(),
            out.arn().unwrap_or_default().to_owned(),
        ))
    }

    async fn ensure_user(&self, user: &IamUser<'_>) -> Result<String> {
        // Create first: IAM authorizes a lookup of a user that doesn't exist against
        // `user/<name>` without its path, which a policy scoped to the path denies.
        let mut create = self
            .iam
            .create_user()
            .user_name(user.name)
            .path(user.path)
            .permissions_boundary(user.boundary);
        for (k, v) in user.tags {
            if let Ok(tag) = aws_sdk_iam::types::Tag::builder().key(k).value(v).build() {
                create = create.tags(tag);
            }
        }
        match create.send().await {
            Ok(out) => return Ok(out.user().map(|u| u.arn().to_owned()).unwrap_or_default()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_entity_already_exists_exception()) => {}
            Err(e) => return Err(self.error(&format!("creating IAM user {}", user.name), e)),
        }
        // It exists (under the path, so this lookup is allowed): only use it with the boundary.
        let out = self
            .iam
            .get_user()
            .user_name(user.name)
            .send()
            .await
            .map_err(|e| self.error(&format!("looking up IAM user {}", user.name), e))?;
        let u = out.user().ok_or_else(|| {
            AppError::Invalid(format!(
                "backend {}: IAM user {} has no details",
                self.backend, user.name
            ))
        })?;
        let boundary = u
            .permissions_boundary()
            .and_then(|b| b.permissions_boundary_arn());
        if boundary != Some(user.boundary) || !u.path().starts_with(user.path) {
            return Err(AppError::Invalid(format!(
                "backend {}: IAM user {} exists, but not under {} with the permissions boundary {}; ikit won't use it",
                self.backend, user.name, user.path, user.boundary
            )));
        }
        Ok(u.arn().to_owned())
    }

    async fn put_user_policy(&self, user: &str, name: &str, document: &str) -> Result<()> {
        self.iam
            .put_user_policy()
            .user_name(user)
            .policy_name(name)
            .policy_document(document)
            .send()
            .await
            .map_err(|e| self.error(&format!("setting the policy of IAM user {user}"), e))?;
        Ok(())
    }

    async fn access_keys(&self, user: &str) -> Result<Vec<String>> {
        let out = self
            .iam
            .list_access_keys()
            .user_name(user)
            .send()
            .await
            .map_err(|e| self.error(&format!("listing the keys of IAM user {user}"), e))?;
        Ok(out
            .access_key_metadata()
            .iter()
            .filter_map(|k| k.access_key_id().map(str::to_owned))
            .collect())
    }

    async fn create_access_key(&self, user: &str) -> Result<Keys> {
        let out = self
            .iam
            .create_access_key()
            .user_name(user)
            .send()
            .await
            .map_err(|e| self.error(&format!("creating a key for IAM user {user}"), e))?;
        let key = out.access_key().ok_or_else(|| {
            AppError::Invalid(format!(
                "backend {}: no key returned for {user}",
                self.backend
            ))
        })?;
        Ok(Keys {
            access_key_id: key.access_key_id().to_owned(),
            secret_access_key: key.secret_access_key().to_owned(),
            session_token: None,
        })
    }

    async fn delete_access_key(&self, user: &str, id: &str) -> Result<()> {
        match self
            .iam
            .delete_access_key()
            .user_name(user)
            .access_key_id(id)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_no_such_entity_exception()) =>
            {
                Ok(())
            }
            Err(e) => Err(self.error(&format!("deleting key {id} of IAM user {user}"), e)),
        }
    }

    async fn delete_user(&self, user: &str) -> Result<()> {
        let policies = match self.iam.list_user_policies().user_name(user).send().await {
            Ok(out) => out.policy_names().to_vec(),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_no_such_entity_exception()) =>
            {
                return Ok(());
            }
            Err(e) => return Err(self.error(&format!("listing policies of IAM user {user}"), e)),
        };
        for p in policies {
            self.iam
                .delete_user_policy()
                .user_name(user)
                .policy_name(&p)
                .send()
                .await
                .map_err(|e| self.error(&format!("deleting policy {p} of IAM user {user}"), e))?;
        }
        for id in self.access_keys(user).await? {
            self.delete_access_key(user, &id).await?;
        }
        self.iam
            .delete_user()
            .user_name(user)
            .send()
            .await
            .map_err(|e| self.error(&format!("deleting IAM user {user}"), e))?;
        Ok(())
    }

    async fn wait_for_key(&self, keys: &Keys) -> Result<()> {
        let conf = aws_sdk_sts::config::Builder::from(&self.conf)
            .credentials_provider(aws_sdk_sts::config::Credentials::new(
                keys.access_key_id.clone(),
                keys.secret_access_key.clone(),
                None,
                None,
                "ikit-new-key",
            ))
            .build();
        let sts = aws_sdk_sts::Client::from_conf(conf);
        let start = std::time::Instant::now();
        loop {
            match sts.get_caller_identity().send().await {
                Ok(_) => return Ok(()),
                Err(e) if start.elapsed() > KEY_WAIT => {
                    return Err(self.error(
                        &format!(
                            "new key {} still not accepted after a minute",
                            keys.access_key_id
                        ),
                        e,
                    ));
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
            }
        }
    }
}
