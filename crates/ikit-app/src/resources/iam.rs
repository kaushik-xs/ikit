//! IAM policy documents for per-service keys: the policy of one service's user, the
//! permissions boundary every such user carries, and what the provisioning identity needs.

use std::collections::BTreeSet;

use ikit_core::{Resource, ResourceApi};
use serde_json::{Value, json};

/// The inline policy on every user ikit creates.
pub(crate) const POLICY_NAME: &str = "ikit-resources";

const S3_BUCKET_ACTIONS: &[&str] = &["s3:ListBucket", "s3:GetBucketLocation"];
const S3_OBJECT_ACTIONS: &[&str] = &[
    "s3:GetObject",
    "s3:PutObject",
    "s3:DeleteObject",
    "s3:AbortMultipartUpload",
];
const SQS_ACTIONS: &[&str] = &[
    "sqs:SendMessage",
    "sqs:ReceiveMessage",
    "sqs:DeleteMessage",
    "sqs:ChangeMessageVisibility",
    "sqs:GetQueueAttributes",
    "sqs:GetQueueUrl",
];

/// `/ikit/<org>/`: every IAM user ikit creates for the org lives under it.
pub(crate) fn user_path(org: &str) -> String {
    format!("/ikit/{org}/")
}

/// A queue's ARN, from its recorded outputs or its name.
fn queue_arn(r: &Resource, region: &str, account: &str) -> String {
    r.outputs
        .get("arn")
        .filter(|a| !a.is_empty())
        .cloned()
        .unwrap_or_else(|| format!("arn:aws:sqs:{region}:{account}:{}", r.external_id))
}

/// The ARNs one resource's statements cover: (bucket-level or queue ARNs, object ARNs).
fn arns(r: &Resource, region: &str, account: &str) -> (Vec<String>, Vec<String>) {
    match r.api {
        ResourceApi::S3 => {
            let bucket = format!("arn:aws:s3:::{}", r.external_id);
            (vec![bucket.clone()], vec![format!("{bucket}/*")])
        }
        ResourceApi::Sqs => {
            let arn = queue_arn(r, region, account);
            let mut queues = vec![arn.clone()];
            if r.outputs.contains_key("dlq_url") {
                queues.push(format!("{arn}-dlq"));
            }
            (queues, vec![])
        }
    }
}

/// The inline policy of a service's user: exactly its resources.
pub(crate) fn service_policy(resources: &[&Resource], region: &str, account: &str) -> Value {
    let mut buckets = BTreeSet::new();
    let mut objects = BTreeSet::new();
    let mut queues = BTreeSet::new();
    for r in resources {
        let (top, inner) = arns(r, region, account);
        match r.api {
            ResourceApi::S3 => {
                buckets.extend(top);
                objects.extend(inner);
            }
            ResourceApi::Sqs => queues.extend(top),
        }
    }
    let mut statements = Vec::new();
    if !buckets.is_empty() {
        statements.push(json!({ "Sid": "Buckets", "Effect": "Allow",
            "Action": S3_BUCKET_ACTIONS, "Resource": buckets }));
        statements.push(json!({ "Sid": "Objects", "Effect": "Allow",
            "Action": S3_OBJECT_ACTIONS, "Resource": objects }));
    }
    if !queues.is_empty() {
        statements.push(json!({ "Sid": "Queues", "Effect": "Allow",
            "Action": SQS_ACTIONS, "Resource": queues }));
    }
    json!({ "Version": "2012-10-17", "Statement": statements })
}

/// The permissions boundary: S3 and SQS on resources named after the org's workloads
/// (`<prefix>-*`), plus `extra` resources adopted from outside that prefix.
pub(crate) fn boundary_policy(prefix: &str, account: &str, extra: &[&Resource]) -> Value {
    let mut buckets: BTreeSet<String> = BTreeSet::from([format!("arn:aws:s3:::{prefix}-*")]);
    let mut objects: BTreeSet<String> = BTreeSet::from([format!("arn:aws:s3:::{prefix}-*/*")]);
    let mut queues: BTreeSet<String> =
        BTreeSet::from([format!("arn:aws:sqs:*:{account}:{prefix}-*")]);
    for r in extra {
        let (top, inner) = arns(r, "*", account);
        match r.api {
            ResourceApi::S3 => {
                buckets.extend(top);
                objects.extend(inner);
            }
            ResourceApi::Sqs => queues.extend(top),
        }
    }
    json!({
        "Version": "2012-10-17",
        "Statement": [
            { "Sid": "IkitBuckets", "Effect": "Allow", "Action": S3_BUCKET_ACTIONS, "Resource": buckets },
            { "Sid": "IkitObjects", "Effect": "Allow", "Action": S3_OBJECT_ACTIONS, "Resource": objects },
            { "Sid": "IkitQueues", "Effect": "Allow", "Action": SQS_ACTIONS, "Resource": queues },
        ]
    })
}

/// What the provisioning identity needs: IAM on the org's user path, creating users and
/// changing their policies only with `boundary` attached, never removing it; and creating the
/// org's buckets and queues.
pub(crate) fn provisioning_policy(org: &str, prefix: &str, account: &str, boundary: &str) -> Value {
    let users = format!("arn:aws:iam::{account}:user{}*", user_path(org));
    json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Sid": "IkitUsers",
                "Effect": "Allow",
                "Action": [
                    "iam:GetUser", "iam:TagUser", "iam:DeleteUser", "iam:ListUserPolicies",
                    "iam:CreateAccessKey", "iam:DeleteAccessKey", "iam:ListAccessKeys"
                ],
                "Resource": users
            },
            {
                "Sid": "IkitBoundedUsers",
                "Effect": "Allow",
                "Action": ["iam:CreateUser", "iam:PutUserPolicy", "iam:DeleteUserPolicy"],
                "Resource": users,
                "Condition": { "StringEquals": { "iam:PermissionsBoundary": boundary } }
            },
            {
                "Sid": "IkitKeepBoundaries",
                "Effect": "Deny",
                "Action": ["iam:DeleteUserPermissionsBoundary", "iam:PutUserPermissionsBoundary"],
                "Resource": "*"
            },
            {
                "Sid": "IkitBuckets",
                "Effect": "Allow",
                "Action": [
                    "s3:CreateBucket", "s3:DeleteBucket", "s3:PutBucketTagging",
                    "s3:PutBucketVersioning", "s3:ListBucket"
                ],
                "Resource": format!("arn:aws:s3:::{prefix}-*")
            },
            {
                "Sid": "IkitQueues",
                "Effect": "Allow",
                "Action": [
                    "sqs:CreateQueue", "sqs:DeleteQueue", "sqs:TagQueue", "sqs:SetQueueAttributes",
                    "sqs:GetQueueAttributes", "sqs:GetQueueUrl"
                ],
                "Resource": format!("arn:aws:sqs:*:{account}:{prefix}-*")
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use ikit_core::{ResourceOrigin, ResourceStatus};

    use super::*;

    fn resource(api: ResourceApi, id: &str, outputs: &[(&str, &str)]) -> Resource {
        Resource {
            id: 1,
            workload: "wl-acme-dev-001".into(),
            service: "sim".into(),
            name: "x".into(),
            api,
            backend: Some("b".into()),
            external_id: id.into(),
            outputs: outputs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            origin: ResourceOrigin::Created,
            status: ResourceStatus::Ready,
            orphaned: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn service_policy_covers_exactly_its_resources() {
        let bucket = resource(ResourceApi::S3, "wl-acme-dev-001-sim-assets-abc", &[]);
        let queue = resource(
            ResourceApi::Sqs,
            "wl-acme-dev-001-sim-jobs",
            &[
                ("arn", "arn:aws:sqs:ap-south-1:1:wl-acme-dev-001-sim-jobs"),
                ("dlq_url", "https://sqs/1/wl-acme-dev-001-sim-jobs-dlq"),
            ],
        );
        let p = service_policy(&[&bucket, &queue], "ap-south-1", "1");
        let s = &p["Statement"];
        assert_eq!(
            s[0]["Resource"][0],
            "arn:aws:s3:::wl-acme-dev-001-sim-assets-abc"
        );
        assert_eq!(
            s[1]["Resource"][0],
            "arn:aws:s3:::wl-acme-dev-001-sim-assets-abc/*"
        );
        assert_eq!(
            s[2]["Resource"],
            json!([
                "arn:aws:sqs:ap-south-1:1:wl-acme-dev-001-sim-jobs",
                "arn:aws:sqs:ap-south-1:1:wl-acme-dev-001-sim-jobs-dlq"
            ])
        );
        // A queue only: no S3 statements at all.
        let p = service_policy(&[&queue], "ap-south-1", "1");
        assert_eq!(p["Statement"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn boundary_and_provisioning_policies() {
        let adopted = resource(ResourceApi::S3, "new-app-dev-simulation-b88b643e", &[]);
        let b = boundary_policy("wl-acme", "123", &[&adopted]);
        let buckets = &b["Statement"][0]["Resource"];
        assert!(
            buckets
                .as_array()
                .unwrap()
                .contains(&json!("arn:aws:s3:::wl-acme-*"))
        );
        assert!(
            buckets
                .as_array()
                .unwrap()
                .contains(&json!("arn:aws:s3:::new-app-dev-simulation-b88b643e"))
        );
        assert_eq!(
            b["Statement"][2]["Resource"][0],
            "arn:aws:sqs:*:123:wl-acme-*"
        );

        let p = provisioning_policy(
            "acme",
            "wl-acme",
            "123",
            "arn:aws:iam::123:policy/ikit-acme-boundary",
        );
        assert_eq!(
            p["Statement"][0]["Resource"],
            "arn:aws:iam::123:user/ikit/acme/*"
        );
        assert_eq!(
            p["Statement"][1]["Condition"]["StringEquals"]["iam:PermissionsBoundary"],
            "arn:aws:iam::123:policy/ikit-acme-boundary"
        );
        assert_eq!(p["Statement"][2]["Effect"], "Deny");
    }
}
