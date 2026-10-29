//! `zerotier-cli -j info` and `zerotier-cli -j listnetworks`.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::{NetError, Result};

/// This node, from `zerotier-cli -j info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// The 10-digit node address; it is the member ID in every network.
    pub address: String,
    pub online: bool,
    pub version: String,
}

/// A network this node has joined, from `zerotier-cli -j listnetworks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalNetwork {
    pub id: String,
    pub name: String,
    /// `OK` once authorized; `REQUESTING_CONFIGURATION` or `ACCESS_DENIED` before.
    pub status: String,
    /// The network interface, e.g. `ztabcdef12` on Linux or `feth1234` on macOS.
    pub device: String,
    /// Assigned addresses in CIDR form, e.g. `10.147.17.5/24`.
    pub addresses: Vec<String>,
}

impl LocalNetwork {
    /// Assigned addresses without their prefix length.
    pub fn ips(&self) -> Vec<IpAddr> {
        self.addresses
            .iter()
            .filter_map(|a| a.split('/').next()?.parse().ok())
            .collect()
    }

    pub fn ipv4(&self) -> Option<IpAddr> {
        self.ips().into_iter().find(IpAddr::is_ipv4)
    }
}

#[derive(Deserialize)]
struct RawInfo {
    address: String,
    #[serde(default)]
    online: bool,
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNetwork {
    // Newer versions print both `id` and `nwid`; older ones only `nwid`.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    nwid: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    port_device_name: String,
    #[serde(default)]
    assigned_addresses: Vec<String>,
}

pub fn parse_info(json: &str) -> Result<NodeInfo> {
    let raw: RawInfo =
        serde_json::from_str(json).map_err(|e| NetError::Parse(format!("info: {e}")))?;
    Ok(NodeInfo {
        address: raw.address,
        online: raw.online,
        version: raw.version,
    })
}

pub fn parse_networks(json: &str) -> Result<Vec<LocalNetwork>> {
    let raw: Vec<RawNetwork> =
        serde_json::from_str(json).map_err(|e| NetError::Parse(format!("listnetworks: {e}")))?;
    raw.into_iter()
        .map(|n| {
            let id = n
                .id
                .or(n.nwid)
                .ok_or_else(|| NetError::Parse("listnetworks: a network has no id".to_owned()))?;
            Ok(LocalNetwork {
                id,
                name: n.name,
                status: n.status,
                device: n.port_device_name,
                addresses: n.assigned_addresses,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_info() {
        let info = parse_info(
            r#"{"address":"a1b2c3d4e5","online":true,"version":"1.14.2","config":{"settings":{}}}"#,
        )
        .unwrap();
        assert_eq!(info.address, "a1b2c3d4e5");
        assert!(info.online);
        assert!(parse_info("not json").is_err());
    }

    #[test]
    fn parses_networks() {
        let nets = parse_networks(
            r#"[{"id":"8056c2e21c000001","nwid":"8056c2e21c000001","name":"acme","status":"OK",
                 "portDeviceName":"zt3jnkd7xa","assignedAddresses":["fd80::1/88","10.147.17.5/24"]},
                {"nwid":"8056c2e21c000002","status":"REQUESTING_CONFIGURATION","assignedAddresses":[]}]"#,
        )
        .unwrap();
        assert_eq!(nets.len(), 2);
        assert_eq!(nets[0].device, "zt3jnkd7xa");
        assert_eq!(nets[0].ipv4(), Some("10.147.17.5".parse().unwrap()));
        assert_eq!(nets[0].ips().len(), 2);
        assert_eq!(nets[1].id, "8056c2e21c000002");
        assert_eq!(nets[1].ipv4(), None);
    }
}
