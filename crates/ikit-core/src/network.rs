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
    pub created_at: DateTime<Utc>,
}
