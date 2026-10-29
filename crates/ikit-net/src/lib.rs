//! ZeroTier for ikit: a client for the Central API (networks and members) and parsers for
//! `zerotier-cli -j` output. Running `zerotier-cli`, locally or over SSH, is left to the
//! caller so it can go through ikit's command runner.

mod central;
mod cidr;
mod cli;

pub use central::{Central, CentralNetwork, Member};
pub use cidr::Ipv4Cidr;
pub use cli::{LocalNetwork, NodeInfo, parse_info, parse_networks};

pub type Result<T, E = NetError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("ZeroTier Central request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ZeroTier Central returned {status}: {message}")]
    Api { status: u16, message: String },
    #[error("unexpected zerotier output: {0}")]
    Parse(String),
    #[error("{0}")]
    Invalid(String),
}

/// Whether `s` is a ZeroTier network ID: 16 hex digits.
pub fn is_network_id(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `s` is a ZeroTier node address: 10 hex digits.
pub fn is_node_id(s: &str) -> bool {
    s.len() == 10 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_ids() {
        assert!(is_network_id("8056c2e21c000001"));
        assert!(!is_network_id("8056c2e21c00000"));
        assert!(!is_network_id("8056c2e21c00000g"));
        assert!(is_node_id("a1b2c3d4e5"));
        assert!(!is_node_id("a1b2c3d4e5f6"));
    }
}
