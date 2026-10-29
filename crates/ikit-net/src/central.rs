//! The ZeroTier Central API (`https://api.zerotier.com/api/v1`): networks and members.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Ipv4Cidr, NetError, Result};

pub struct Central {
    http: reqwest::Client,
    base: String,
    token: String,
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
#[serde(rename_all = "camelCase")]
struct RawNetwork {
    id: String,
    #[serde(default)]
    config: RawNetworkConfig,
    #[serde(default)]
    authorized_member_count: u64,
    #[serde(default)]
    online_member_count: u64,
    #[serde(default)]
    total_member_count: u64,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNetworkConfig {
    #[serde(default)]
    name: String,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    routes: Vec<RawRoute>,
    #[serde(default)]
    ip_assignment_pools: Vec<RawPool>,
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
    node_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    online: bool,
    #[serde(default)]
    client_version: Option<String>,
    #[serde(default)]
    physical_address: Option<String>,
    #[serde(default)]
    config: RawMemberConfig,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMemberConfig {
    #[serde(default)]
    authorized: bool,
    #[serde(default)]
    ip_assignments: Vec<String>,
}

impl From<RawNetwork> for CentralNetwork {
    fn from(n: RawNetwork) -> Self {
        Self {
            id: n.id,
            name: n.config.name,
            private: n.config.private,
            routes: n.config.routes.into_iter().map(|r| r.target).collect(),
            pools: n
                .config
                .ip_assignment_pools
                .into_iter()
                .map(|p| format!("{}-{}", p.ip_range_start, p.ip_range_end))
                .collect(),
            authorized_members: n.authorized_member_count,
            online_members: n.online_member_count,
            total_members: n.total_member_count,
        }
    }
}

impl From<RawMember> for Member {
    fn from(m: RawMember) -> Self {
        Self {
            node_id: m.node_id,
            name: m.name.unwrap_or_default(),
            authorized: m.config.authorized,
            online: m.online,
            ips: m.config.ip_assignments,
            client_version: m.client_version.unwrap_or_default(),
            physical_address: m.physical_address.unwrap_or_default(),
        }
    }
}

impl Central {
    /// `base` is the API root, e.g. `https://api.zerotier.com/api/v1`.
    pub fn new(base: &str, token: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("ikit/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client builds");
        Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = req
            .header("Authorization", format!("token {}", self.token))
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| body.trim().chars().take(200).collect());
        let message = match status.as_u16() {
            401 | 403 if message.is_empty() => {
                "the API token was rejected (check the zerotier profile's api_token)".to_owned()
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

    /// Creates a private network that assigns addresses from `cidr` and routes it.
    pub async fn create_network(&self, name: &str, cidr: &Ipv4Cidr) -> Result<CentralNetwork> {
        let raw: RawNetwork = self.post("/network", network_body(name, cidr)).await?;
        Ok(raw.into())
    }

    pub async fn network(&self, id: &str) -> Result<CentralNetwork> {
        let raw: RawNetwork = self.get(&format!("/network/{id}")).await?;
        Ok(raw.into())
    }

    pub async fn delete_network(&self, id: &str) -> Result<()> {
        let url = format!("{}/network/{id}", self.base);
        self.send(self.http.delete(url)).await?;
        Ok(())
    }

    pub async fn members(&self, network: &str) -> Result<Vec<Member>> {
        let raw: Vec<RawMember> = self.get(&format!("/network/{network}/member")).await?;
        let mut members: Vec<Member> = raw.into_iter().map(Member::from).collect();
        members.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        Ok(members)
    }

    /// Authorizes or deauthorizes `node` and optionally names it. Central creates the
    /// member if the node has not asked to join yet, so a node can be authorized ahead.
    pub async fn update_member(
        &self,
        network: &str,
        node: &str,
        authorized: Option<bool>,
        name: Option<&str>,
    ) -> Result<Member> {
        let raw: RawMember = self
            .post(
                &format!("/network/{network}/member/{node}"),
                member_body(authorized, name),
            )
            .await?;
        Ok(raw.into())
    }
}

fn network_body(name: &str, cidr: &Ipv4Cidr) -> serde_json::Value {
    let (start, end) = cidr.pool();
    json!({
        "config": {
            "name": name,
            "private": true,
            "v4AssignMode": { "zt": true },
            "ipAssignmentPools": [{ "ipRangeStart": start.to_string(), "ipRangeEnd": end.to_string() }],
            "routes": [{ "target": cidr.to_string(), "via": null }],
        }
    })
}

fn member_body(authorized: Option<bool>, name: Option<&str>) -> serde_json::Value {
    let mut body = json!({ "config": {} });
    if let Some(a) = authorized {
        body["config"]["authorized"] = json!(a);
    }
    if let Some(n) = name {
        body["name"] = json!(n);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_request_bodies() {
        let body = network_body("acme-dev", &"10.147.17.0/24".parse().unwrap());
        assert_eq!(body["config"]["private"], true);
        assert_eq!(
            body["config"]["ipAssignmentPools"][0]["ipRangeEnd"],
            "10.147.17.254"
        );
        assert_eq!(body["config"]["routes"][0]["target"], "10.147.17.0/24");

        let body = member_body(Some(true), None);
        assert_eq!(body, json!({ "config": { "authorized": true } }));
        let body = member_body(None, Some("box"));
        assert_eq!(body, json!({ "config": {}, "name": "box" }));
    }

    #[test]
    fn parses_api_responses() {
        let n: RawNetwork = serde_json::from_str(
            r#"{"id":"8056c2e21c000001","config":{"name":"acme","private":true,
                "routes":[{"target":"10.147.17.0/24","via":null}],
                "ipAssignmentPools":[{"ipRangeStart":"10.147.17.1","ipRangeEnd":"10.147.17.254"}]},
                "authorizedMemberCount":2,"onlineMemberCount":1,"totalMemberCount":3}"#,
        )
        .unwrap();
        let n = CentralNetwork::from(n);
        assert_eq!(n.routes, ["10.147.17.0/24"]);
        assert_eq!(n.pools, ["10.147.17.1-10.147.17.254"]);
        assert_eq!(n.total_members, 3);

        let m: RawMember = serde_json::from_str(
            r#"{"id":"8056c2e21c000001-a1b2c3d4e5","nodeId":"a1b2c3d4e5","name":null,"online":true,
                "clientVersion":"1.14.2","config":{"authorized":true,"ipAssignments":["10.147.17.5"]}}"#,
        )
        .unwrap();
        let m = Member::from(m);
        assert!(m.authorized);
        assert_eq!(m.ips, ["10.147.17.5"]);
        assert_eq!(m.name, "");
    }
}
