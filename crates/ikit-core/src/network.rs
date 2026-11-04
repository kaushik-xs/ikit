//! ZeroTier networks: created through the Central API or attached by network ID, and
//! optionally linked to workloads.

use chrono::{DateTime, Utc};
use serde::Serialize;

pub use crate::target::TargetOrigin as NetworkOrigin;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Network {
    pub id: i64,
    pub name: String,
    /// The 16-digit ZeroTier network ID.
    pub network_id: String,
    /// The zerotier profile holding the Central API token.
    pub profile: String,
    /// `created` networks are deleted from Central with `ikit net zt rm --delete`.
    pub origin: NetworkOrigin,
    /// Managed route and address pool, for networks ikit created.
    pub cidr: Option<String>,
    /// Workloads linked to this network.
    pub workloads: Vec<String>,
    /// The linked workloads that run a ZeroTier sidecar container.
    pub sidecars: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// A Docker network outside the workload that one of its service groups joins as
/// `external: true`. ikit never creates or removes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DockerNetwork {
    pub name: String,
    pub group: crate::Category,
}

/// Whether `name` is a valid Docker network name. The group names are refused so an
/// external network can never shadow a group's own network in the compose file.
pub fn is_docker_network_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        && !["app", "platform", "data"].contains(&name)
}
