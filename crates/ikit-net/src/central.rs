//! The ZeroTier Central API v2 (`https://central.zerotier.com/api/v2`): networks and members,
//! authenticated with a service account's API key.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Ipv4Cidr, NetError, Result};

/// A member counts as online when Central saw it this recently.
const ONLINE_WINDOW: chrono::Duration = chrono::Duration::minutes(5);

pub struct Central {
    http: reqwest::Client,
    base: String,
    token: String,
    /// The New Central organization; needed to create networks.
    org_id: Option<String>,
    /// The network group new networks go in (default: the org's only group).
    network_group: Option<String>,
}

/// A network as Central reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CentralNetwork {
    pub id: String,
    pub name: String,
    /// Members must be authorized before they get an address.
    pub private: bool,
    /// Managed routes, e.g. `10.147.17.0/24`.
    pub routes: Vec<String>,
    /// Address pools as `start-end`.
    pub pools: Vec<String>,
    pub authorized_members: u64,
    pub online_members: u64,
    pub total_members: u64,
}

/// A node that joined (or asked to join) a network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    /// The node address.
    pub node_id: String,
    pub name: String,
    pub authorized: bool,
    pub online: bool,
    /// Managed addresses on this network.
    pub ips: Vec<String>,
    pub client_version: String,
    pub physical_address: String,
}

#[derive(Deserialize)]
struct RawNetwork {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    config: RawNetworkConfig,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNetworkConfig {
    #[serde(default)]
    private: bool,
    #[serde(default)]
    routes: Vec<RawRoute>,
    #[serde(default)]
    v4_ip_assignment_pools: Vec<RawPool>,
}

#[derive(Deserialize)]
struct RawRoute {
    target: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPool {
    ip_range_start: String,
    ip_range_end: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMember {
    device_id: String,
    #[serde(default)]
    name: String,
    /// `authorized`, `not_authorized` or `rejected`.
    #[serde(default)]
    status: String,
    #[serde(default)]
    ipv4_assignments: Vec<String>,
    #[serde(default)]
    agent_version: Option<String>,
    #[serde(default)]
    last_seen_ip: Option<String>,
    #[serde(default)]
    last_seen_time: Option<String>,
}

#[derive(Deserialize)]
struct RawGroups {
    #[serde(default)]
    items: Vec<RawGroup>,
}

#[derive(Deserialize)]
struct RawGroup {
    id: String,
    #[serde(default)]
    name: String,
}

impl RawNetwork {
    fn into_network(self, members: &[Member]) -> CentralNetwork {
        CentralNetwork {
            id: self.id,
            name: self.name,
            private: self.config.private,
            routes: self.config.routes.into_iter().map(|r| r.target).collect(),
            pools: self
                .config
                .v4_ip_assignment_pools
                .into_iter()
                .map(|p| format!("{}-{}", p.ip_range_start, p.ip_range_end))
                .collect(),
            authorized_members: members.iter().filter(|m| m.authorized).count() as u64,
            online_members: members.iter().filter(|m| m.online).count() as u64,
            total_members: members.len() as u64,
        }
    }
}

impl RawMember {
    fn into_member(self, now: DateTime<Utc>) -> Member {
        let online = self
            .last_seen_time
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| now - t.with_timezone(&Utc) < ONLINE_WINDOW);
        Member {
            node_id: self.device_id,
            name: self.name,
            authorized: self.status == "authorized",
            online,
            ips: self.ipv4_assignments,
            client_version: self.agent_version.unwrap_or_default(),
            physical_address: self.last_seen_ip.unwrap_or_default(),
        }
    }
}

impl Central {
    /// `base` is the API root, e.g. `https://central.zerotier.com/api/v2`; `token` is a
    /// service account's API key.
    pub fn new(base: &str, token: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("ikit/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client builds");
        Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token: token.trim().to_owned(),
            org_id: None,
            network_group: None,
        }
    }

    /// The organization and (optionally) network group that new networks are created in.
    pub fn with_org(mut self, org_id: Option<&str>, network_group: Option<&str>) -> Self {
        self.org_id = org_id.map(str::to_owned);
        self.network_group = network_group.map(str::to_owned);
        self
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = req.bearer_auth(&self.token).send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                ["message", "error", "detail"]
                    .iter()
                    .find_map(|k| v[k].as_str().map(str::to_owned))
            })
            .unwrap_or_else(|| body.trim().chars().take(200).collect());
        let message = match status.as_u16() {
            401 => format!(
                "{} (the zerotier profile's api_token must be a New Central service account \
                 API key)",
                if message.is_empty() {
                    "unauthorized"
                } else {
                    &message
                }
            ),
            403 if message.is_empty() => {
                "the service account has no access to this (add it to the network's or \
                 organization's IAM roles)"
                    .to_owned()
            }
            404 if message.is_empty() => "not found".to_owned(),
            _ => message,
        };
        Err(NetError::Api {
            status: status.as_u16(),
            message,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base);
        Ok(self.send(self.http.get(url)).await?.json().await?)
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T> {
        let url = format!("{}{path}", self.base);
        Ok(self
            .send(self.http.post(url).json(&body))
            .await?
            .json()
            .await?)
    }

    /// The network group to create networks in: the configured one, else the org's only
    /// group, else a new `ikit` group when the org has none.
    async fn network_group(&self) -> Result<String> {
        if let Some(g) = &self.network_group {
            return Ok(g.clone());
        }
        let org = self.org_id.as_deref().ok_or_else(|| {
            NetError::Invalid(
                "creating a network needs the organization: set org_id on the zerotier profile"
                    .to_owned(),
            )
        })?;
        let groups: RawGroups = self.get(&format!("/network-group?org-id={org}")).await?;
        match groups.items.as_slice() {
            [] => {
                let g: RawGroup = self
                    .post(
                        &format!("/org/{org}/network-group"),
                        json!({ "name": "ikit", "description": "Networks created by ikit" }),
                    )
                    .await?;
                Ok(g.id)
            }
            [g] => Ok(g.id.clone()),
            many => Err(NetError::Invalid(format!(
                "organization {org} has several network groups ({}); set network_group on the \
                 zerotier profile to one of their IDs",
                many.iter()
                    .map(|g| format!("{} {}", g.id, g.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// Creates a private network that assigns addresses from `cidr` and routes it.
    pub async fn create_network(&self, name: &str, cidr: &Ipv4Cidr) -> Result<CentralNetwork> {
        let group = self.network_group().await?;
        let raw: RawNetwork = self
            .post(
                &format!("/network-group/{group}/network"),
                network_body(name, cidr),
            )
            .await?;
        Ok(raw.into_network(&[]))
    }

    /// A network with member counts (v2 reports none, so they come from its members).
    pub async fn network(&self, id: &str) -> Result<CentralNetwork> {
        let raw: RawNetwork = self.get(&format!("/network/{id}")).await?;
        let members = self.members(id).await?;
        Ok(raw.into_network(&members))
    }

    pub async fn delete_network(&self, id: &str) -> Result<()> {
        let url = format!("{}/network/{id}", self.base);
        self.send(self.http.delete(url)).await?;
        Ok(())
    }

    pub async fn members(&self, network: &str) -> Result<Vec<Member>> {
        let raw: Vec<RawMember> = self.get(&format!("/network/{network}/member")).await?;
        let now = Utc::now();
        let mut members: Vec<Member> = raw.into_iter().map(|m| m.into_member(now)).collect();
        members.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        Ok(members)
    }

    /// Authorizes or deauthorizes `node` and optionally names it. The node is added as a
    /// member first if it has not asked to join yet, so it can be authorized ahead.
    pub async fn update_member(
        &self,
        network: &str,
        node: &str,
        authorized: Option<bool>,
        name: Option<&str>,
    ) -> Result<Member> {
        let path = format!("/network/{network}/member/{node}");
        let mut raw: RawMember = match self.get::<RawMember>(&path).await {
            Ok(m) => {
                if let Some(n) = name {
                    self.post(&path, json!({ "name": n })).await?
                } else {
                    m
                }
            }
            Err(NetError::Api { status: 404, .. }) => {
                let added: Vec<RawMember> = self
                    .post(
                        &format!("/network/{network}/member"),
                        add_member_body(node, name),
                    )
                    .await?;
                added.into_iter().next().ok_or_else(|| {
                    NetError::Invalid(format!("Central did not add {node} to {network}"))
                })?
            }
            Err(e) => return Err(e),
        };
        if let Some(a) = authorized {
            let action = if a { "authorize" } else { "de-authorize" };
            raw = self.post(&format!("{path}/{action}"), json!({})).await?;
        }
        Ok(raw.into_member(Utc::now()))
    }
}

fn network_body(name: &str, cidr: &Ipv4Cidr) -> serde_json::Value {
    let (start, end) = cidr.pool();
    json!({
        "name": name,
        "config": {
            "private": true,
            "v4AssignmentMode": { "zt": true },
            "v4IpAssignmentPools": [{ "ipRangeStart": start.to_string(), "ipRangeEnd": end.to_string() }],
            "routes": [{ "target": cidr.to_string() }],
        }
    })
}

fn add_member_body(node: &str, name: Option<&str>) -> serde_json::Value {
    let mut member = json!({ "deviceId": node });
    if let Some(n) = name {
        member["name"] = json!(n);
    }
    json!([member])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_request_bodies() {
        let body = network_body("acme-dev", &"10.147.17.0/24".parse().unwrap());
        assert_eq!(body["name"], "acme-dev");
        assert_eq!(body["config"]["private"], true);
        assert_eq!(
            body["config"]["v4IpAssignmentPools"][0]["ipRangeEnd"],
            "10.147.17.254"
        );
        assert_eq!(body["config"]["routes"][0]["target"], "10.147.17.0/24");

        assert_eq!(
            add_member_body("a1b2c3d4e5", None),
            json!([{ "deviceId": "a1b2c3d4e5" }])
        );
        assert_eq!(
            add_member_body("a1b2c3d4e5", Some("box")),
            json!([{ "deviceId": "a1b2c3d4e5", "name": "box" }])
        );
    }

    #[test]
    fn parses_api_responses() {
        let m: RawMember = serde_json::from_str(
            r#"{"deviceId":"a1b2c3d4e5","networkId":"8056c2e21c000001","name":"box",
                "status":"authorized","ipv4Assignments":["10.147.17.5"],"agentVersion":"1.16.1",
                "lastSeenIp":"203.0.113.7","lastSeenTime":"2026-11-05T10:00:00Z"}"#,
        )
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-11-05T10:02:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let m = m.into_member(now);
        assert!(m.authorized && m.online);
        assert_eq!(m.ips, ["10.147.17.5"]);
        assert_eq!(m.physical_address, "203.0.113.7");

        let stale: RawMember = serde_json::from_str(
            r#"{"deviceId":"b1b2c3d4e5","status":"not_authorized","lastSeenTime":"2026-11-05T09:00:00Z"}"#,
        )
        .unwrap();
        let stale = stale.into_member(now);
        assert!(!stale.authorized && !stale.online);

        let n: RawNetwork = serde_json::from_str(
            r#"{"id":"8056c2e21c000001","name":"acme","networkGroupId":"g1","config":{"private":true,
                "routes":[{"target":"10.147.17.0/24","via":""}],
                "v4IpAssignmentPools":[{"ipRangeStart":"10.147.17.1","ipRangeEnd":"10.147.17.254"}]}}"#,
        )
        .unwrap();
        let n = n.into_network(&[m, stale]);
        assert_eq!(n.name, "acme");
        assert_eq!(n.routes, ["10.147.17.0/24"]);
        assert_eq!(n.pools, ["10.147.17.1-10.147.17.254"]);
        assert_eq!(
            (n.authorized_members, n.online_members, n.total_members),
            (1, 1, 2)
        );
    }
}
