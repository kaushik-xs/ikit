//! ZeroTier: networks managed through the Central API (create or attach, members and
//! authorization, links to workloads), and nodes driven with `zerotier-cli` here or on an
//! SSH target (join, leave, status).

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use ikit_core::{Connection, Network, NetworkOrigin, ProfileKind, Target};
use ikit_net::{Central, CentralNetwork, Ipv4Cidr, LocalNetwork, Member, NodeInfo};
use ikit_state::NewNetwork;
use serde::Serialize;

use crate::deploy::sh;
use crate::targets::ssh_args;
use crate::{App, AppError, Result};

const DEFAULT_API: &str = "https://api.zerotier.com/api/v1";

const ZT_TIMEOUT: Duration = Duration::from_secs(30);

/// The ZeroTier node on a machine and the networks it joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZtNode {
    pub node: NodeInfo,
    pub networks: Vec<LocalNetwork>,
}

impl App {
    /// Runs `zerotier-cli -j <args>` here, or on `target` over SSH. It falls back to
    /// `sudo -n` where the CLI cannot read the service's auth token as this user.
    pub(crate) async fn zerotier_cli(
        &self,
        target: Option<&Target>,
        args: &[&str],
    ) -> Result<String> {
        let cmd = std::iter::once("zerotier-cli -j".to_owned())
            .chain(args.iter().map(|a| sh(a)))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "command -v zerotier-cli >/dev/null 2>&1 || exit 127; \
             if zerotier-cli -j info >/dev/null 2>&1; then {cmd}; else sudo -n {cmd}; fi"
        );
        let (program, all) = match target.map(|t| &t.connection) {
            None | Some(Connection::Local) => ("sh", vec!["-c".to_owned(), script]),
            Some(c @ Connection::Ssh { .. }) => {
                let mut all = ssh_args(c);
                all.push(script);
                ("ssh", all)
            }
            Some(Connection::K8s { .. }) => {
                return Err(AppError::Invalid(format!(
                    "target `{}` is a Kubernetes cluster; ZeroTier runs on VM targets",
                    target.map(|t| t.name.as_str()).unwrap_or_default()
                )));
            }
        };
        let place = target.map_or_else(
            || "this machine".to_owned(),
            |t| format!("target `{}`", t.name),
        );
        let out = match self.exec.run(program, &all, None, ZT_TIMEOUT).await {
            Ok(out) => out,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(AppError::Invalid(format!("`{program}` not found on PATH")));
            }
            Err(e) => return Err(e.into()),
        };
        match out.code {
            Some(0) => Ok(out.stdout),
            Some(127) => Err(AppError::Invalid(format!(
                "zerotier-cli is not installed on {place}; install ZeroTier One (https://www.zerotier.com/download/)"
            ))),
            _ => Err(AppError::Invalid(format!(
                "zerotier-cli {} failed on {place}: {}. If it cannot read authtoken.secret, run ikit with sudo \
                 or copy the token to ~/.zeroTierOneAuthToken",
                args.join(" "),
                out.failure_detail()
            ))),
        }
    }

    /// The ZeroTier node on this machine or `target`, and the networks it joined.
    pub async fn zt_node(&self, target: Option<&Target>) -> Result<ZtNode> {
        let info = self.zerotier_cli(target, &["info"]).await?;
        let networks = self.zerotier_cli(target, &["listnetworks"]).await?;
        Ok(ZtNode {
            node: ikit_net::parse_info(&info).map_err(net)?,
            networks: ikit_net::parse_networks(&networks).map_err(net)?,
        })
    }

    /// Resolves where `ikit mcp serve --http` listens. `spec` is `HOST:PORT`, where HOST is
    /// `localhost` or a loopback address, a ZeroTier address of this machine, `zt` (the
    /// first joined ZeroTier network), a ZeroTier network ID, or a ZeroTier interface name.
    /// Anything else is refused: MCP over HTTP is only served on loopback or ZeroTier.
    pub async fn mcp_http_addr(&self, spec: &str) -> Result<SocketAddr> {
        let invalid =
            |why: String| AppError::Invalid(format!("invalid --http address `{spec}`: {why}"));
        let (host, port) = spec.rsplit_once(':').ok_or_else(|| {
            invalid("expected HOST:PORT, e.g. 127.0.0.1:7420 or zt:7420".to_owned())
        })?;
        let port: u16 = port
            .parse()
            .map_err(|_| invalid(format!("bad port `{port}`")))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host == "localhost" {
            return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            if ip.is_loopback() {
                return Ok(SocketAddr::new(ip, port));
            }
            if ip.is_unspecified() {
                return Err(invalid(
                    "binding every interface is not allowed; use localhost or a ZeroTier address"
                        .to_owned(),
                ));
            }
            let node = self.zt_node(None).await.map_err(|e| {
                invalid(format!(
                    "{ip} is not a loopback address, and ZeroTier is unavailable: {e}"
                ))
            })?;
            if node.networks.iter().any(|n| n.ips().contains(&ip)) {
                return Ok(SocketAddr::new(ip, port));
            }
            return Err(invalid(format!(
                "{ip} is neither a loopback address nor a ZeroTier address of this machine"
            )));
        }
        let node = self.zt_node(None).await?;
        let network = node
            .networks
            .iter()
            .filter(|n| n.status == "OK")
            .find(|n| host == "zt" || n.id == host || n.device == host)
            .ok_or_else(|| {
                let joined: Vec<String> = node
                    .networks
                    .iter()
                    .map(|n| format!("{} ({}, {})", n.id, n.device, n.status))
                    .collect();
                invalid(format!(
                    "no joined ZeroTier network matches `{host}`; joined: {}",
                    if joined.is_empty() {
                        "none".to_owned()
                    } else {
                        joined.join(", ")
                    }
                ))
            })?;
        let ip = network
            .ipv4()
            .or_else(|| network.ips().into_iter().next())
            .ok_or_else(|| {
                invalid(format!(
                    "ZeroTier network {} has no address yet; is this node authorized?",
                    network.id
                ))
            })?;
        Ok(SocketAddr::new(ip, port))
    }
}

/// A network with what Central reports about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZtNetworkDetail {
    pub network: Network,
    pub central: CentralNetwork,
    pub members: Vec<Member>,
}

/// What `zt_join` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZtJoin {
    pub network: String,
    pub network_id: String,
    /// The target that joined, or `None` for this machine.
    pub target: Option<String>,
    pub node_id: String,
    /// The member after authorizing it, when asked to.
    pub member: Option<Member>,
}

/// Network names are slugs; an ID is 16 hex digits.
fn check_name(name: &str) -> Result<()> {
    if ikit_core::is_slug(name) {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "invalid network name `{name}`: use {}",
            ikit_core::SLUG_RULE
        )))
    }
}

fn check_node(node: &str) -> Result<()> {
    if ikit_net::is_node_id(node) {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "invalid ZeroTier node ID `{node}`: expected 10 hex digits (see `zerotier-cli info`)"
        )))
    }
}

impl App {
    /// A Central client from a zerotier profile.
    async fn central(&self, profile: &str) -> Result<Central> {
        let values = self.profile_values(ProfileKind::Zerotier, profile).await?;
        let token = values.get("api_token").ok_or_else(|| {
            AppError::Invalid(format!("profile zerotier/{profile} has no api_token"))
        })?;
        let base = values.get("api_url").map_or(DEFAULT_API, String::as_str);
        Ok(Central::new(base, token))
    }

    pub async fn zt_show_network(&self, name: &str) -> Result<Network> {
        self.store
            .networks()
            .get(name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("network `{name}` (see `ikit net zt list`)")))
    }

    pub async fn zt_list(&self) -> Result<Vec<Network>> {
        Ok(self.store.networks().list().await?)
    }

    /// The first `10.147.N.0/24` (from N = 17) no other network in the org uses.
    async fn free_cidr(&self) -> Result<Ipv4Cidr> {
        let used: Vec<String> = self
            .zt_list()
            .await?
            .into_iter()
            .filter_map(|n| n.cidr)
            .collect();
        (17..=254)
            .map(|n| format!("10.147.{n}.0/24"))
            .find(|c| !used.contains(c))
            .ok_or_else(|| {
                AppError::Invalid("no free 10.147.x.0/24 range; pass --cidr".to_owned())
            })?
            .parse()
            .map_err(net)
    }

    /// Creates a private network in Central that assigns addresses from `cidr` (default: a
    /// free 10.147.x.0/24), and links `workload` to it if given.
    pub async fn zt_create(
        &self,
        name: &str,
        profile: &str,
        cidr: Option<Ipv4Cidr>,
        workload: Option<&str>,
    ) -> Result<Network> {
        check_name(name)?;
        if self.store.networks().get(name).await?.is_some() {
            return Err(AppError::AlreadyExists(format!("network `{name}`")));
        }
        let wl = match workload {
            Some(w) => Some(self.workload_get(w).await?),
            None => None,
        };
        let cidr = match cidr {
            Some(c) => c,
            None => self.free_cidr().await?,
        };
        let central = self.central(profile).await?;
        let label = format!("{}-{name}", self.org.name);
        let created = central.create_network(&label, &cidr).await.map_err(net)?;
        let cidr = cidr.to_string();
        let network = self
            .store
            .networks()
            .insert(NewNetwork {
                name,
                network_id: &created.id,
                profile,
                origin: NetworkOrigin::Created,
                cidr: Some(&cidr),
            })
            .await?;
        if let Some(wl) = wl {
            self.store.networks().link(wl.id, network.id).await?;
        }
        self.zt_show_network(name).await
    }

    /// Registers an existing network by ID after checking the profile can read it.
    pub async fn zt_attach(
        &self,
        name: &str,
        network_id: &str,
        profile: &str,
        workload: Option<&str>,
    ) -> Result<Network> {
        check_name(name)?;
        if !ikit_net::is_network_id(network_id) {
            return Err(AppError::Invalid(format!(
                "invalid ZeroTier network ID `{network_id}`: expected 16 hex digits"
            )));
        }
        let wl = match workload {
            Some(w) => Some(self.workload_get(w).await?),
            None => None,
        };
        let central = self.central(profile).await?;
        let found = central.network(network_id).await.map_err(net)?;
        let network = self
            .store
            .networks()
            .insert(NewNetwork {
                name,
                network_id: &found.id,
                profile,
                origin: NetworkOrigin::Attached,
                cidr: found.routes.first().map(String::as_str),
            })
            .await?;
        if let Some(wl) = wl {
            self.store.networks().link(wl.id, network.id).await?;
        }
        self.zt_show_network(name).await
    }

    /// The network with its Central config and members.
    pub async fn zt_show(&self, name: &str) -> Result<ZtNetworkDetail> {
        let network = self.zt_show_network(name).await?;
        let central = self.central(&network.profile).await?;
        let config = central.network(&network.network_id).await.map_err(net)?;
        let members = central.members(&network.network_id).await.map_err(net)?;
        Ok(ZtNetworkDetail {
            network,
            central: config,
            members,
        })
    }

    pub async fn zt_members(&self, name: &str) -> Result<Vec<Member>> {
        let network = self.zt_show_network(name).await?;
        let central = self.central(&network.profile).await?;
        central.members(&network.network_id).await.map_err(net)
    }

    /// Authorizes (or deauthorizes) node `node` and optionally names it. A node can be
    /// authorized before it joins.
    pub async fn zt_authorize(
        &self,
        name: &str,
        node: &str,
        authorized: bool,
        member_name: Option<&str>,
    ) -> Result<Member> {
        check_node(node)?;
        let network = self.zt_show_network(name).await?;
        let central = self.central(&network.profile).await?;
        central
            .update_member(&network.network_id, node, Some(authorized), member_name)
            .await
            .map_err(net)
    }

    /// Joins this machine, or `target`, to the network; with `authorize`, also authorizes
    /// it in Central, named after the target (or `member_name`).
    pub async fn zt_join(
        &self,
        name: &str,
        target: Option<&str>,
        authorize: bool,
        member_name: Option<&str>,
    ) -> Result<ZtJoin> {
        let network = self.zt_show_network(name).await?;
        let target = match target {
            Some(t) => Some(self.target_show(t).await?),
            None => None,
        };
        // Check the profile before changing the node, so a bad token fails early.
        let central = if authorize {
            Some(self.central(&network.profile).await?)
        } else {
            None
        };
        self.zerotier_cli(target.as_ref(), &["join", &network.network_id])
            .await?;
        let info = ikit_net::parse_info(&self.zerotier_cli(target.as_ref(), &["info"]).await?)
            .map_err(net)?;
        let member = match central {
            Some(c) => {
                let label = member_name
                    .map(str::to_owned)
                    .or_else(|| target.as_ref().map(|t| t.name.clone()));
                Some(
                    c.update_member(
                        &network.network_id,
                        &info.address,
                        Some(true),
                        label.as_deref(),
                    )
                    .await
                    .map_err(net)?,
                )
            }
            None => None,
        };
        Ok(ZtJoin {
            network: network.name,
            network_id: network.network_id,
            target: target.map(|t| t.name),
            node_id: info.address,
            member,
        })
    }

    /// Leaves the network on this machine or `target`; with `deauthorize`, also revokes the
    /// node in Central. Returns the node ID.
    pub async fn zt_leave(
        &self,
        name: &str,
        target: Option<&str>,
        deauthorize: bool,
    ) -> Result<String> {
        let network = self.zt_show_network(name).await?;
        let target = match target {
            Some(t) => Some(self.target_show(t).await?),
            None => None,
        };
        let info = ikit_net::parse_info(&self.zerotier_cli(target.as_ref(), &["info"]).await?)
            .map_err(net)?;
        self.zerotier_cli(target.as_ref(), &["leave", &network.network_id])
            .await?;
        if deauthorize {
            let central = self.central(&network.profile).await?;
            central
                .update_member(&network.network_id, &info.address, Some(false), None)
                .await
                .map_err(net)?;
        }
        Ok(info.address)
    }

    /// Links `workload` to the network, replacing the network it was linked to.
    pub async fn zt_link(&self, name: &str, workload: &str) -> Result<Network> {
        let network = self.zt_show_network(name).await?;
        let wl = self.workload_get(workload).await?;
        self.store.networks().link(wl.id, network.id).await?;
        self.zt_show_network(name).await
    }

    pub async fn zt_unlink(&self, workload: &str) -> Result<()> {
        let wl = self.workload_get(workload).await?;
        if !self.store.networks().unlink(wl.id).await? {
            return Err(AppError::NotFound(format!(
                "a network linked to {workload}"
            )));
        }
        Ok(())
    }

    /// The network `workload` is linked to, if any.
    pub async fn zt_for_workload(&self, workload: &str) -> Result<Option<Network>> {
        let wl = self.workload_get(workload).await?;
        Ok(self.store.networks().for_workload(wl.id).await?)
    }

    /// Removes the network from ikit; with `delete`, also deletes it in Central (only for
    /// networks ikit created). Refused while workloads are linked to it.
    pub async fn zt_remove(&self, name: &str, delete: bool) -> Result<()> {
        let network = self.zt_show_network(name).await?;
        if !network.workloads.is_empty() {
            return Err(AppError::InUse {
                what: format!("network `{name}`"),
                by: network.workloads,
            });
        }
        if delete && network.origin != NetworkOrigin::Created {
            return Err(AppError::Invalid(format!(
                "network `{name}` was attached, not created by ikit; delete it in ZeroTier Central instead"
            )));
        }
        if delete {
            let central = self.central(&network.profile).await?;
            match central.delete_network(&network.network_id).await {
                Ok(()) | Err(ikit_net::NetError::Api { status: 404, .. }) => {}
                Err(e) => return Err(net(e)),
            }
        }
        self.store.networks().delete(name).await?;
        Ok(())
    }
}

pub(crate) fn net(e: ikit_net::NetError) -> AppError {
    AppError::Invalid(e.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::testing::app;
    use crate::{Exec, ExecOutput};

    /// Answers `info` and `listnetworks` with canned JSON and records the scripts it ran.
    #[derive(Default)]
    struct FakeZt(Mutex<Vec<String>>);

    #[async_trait]
    impl Exec for FakeZt {
        async fn run(
            &self,
            _program: &str,
            args: &[String],
            _stdin: Option<&[u8]>,
            _timeout: Duration,
        ) -> io::Result<ExecOutput> {
            let script = args.last().cloned().unwrap_or_default();
            self.0.lock().unwrap().push(script.clone());
            let stdout = if script.contains("'listnetworks'") {
                r#"[{"id":"8056c2e21c000001","name":"acme","status":"OK","portDeviceName":"ztabc",
                     "assignedAddresses":["10.147.17.5/24"]}]"#
            } else {
                r#"{"address":"a1b2c3d4e5","online":true,"version":"1.14.2"}"#
            };
            Ok(ExecOutput {
                code: Some(0),
                stdout: stdout.to_owned(),
                ..Default::default()
            })
        }
    }

    /// An in-memory ZeroTier Central: one network ID per create, members by node ID.
    async fn fake_central() -> String {
        use axum::extract::{Path, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use serde_json::{Value, json};

        #[derive(Default)]
        struct Db {
            networks: std::collections::BTreeMap<String, Value>,
            members: std::collections::BTreeMap<(String, String), Value>,
        }
        type Shared = Arc<Mutex<Db>>;

        fn authed(h: &HeaderMap) -> Result<(), StatusCode> {
            (h.get("authorization").and_then(|v| v.to_str().ok()) == Some("token secret"))
                .then_some(())
                .ok_or(StatusCode::UNAUTHORIZED)
        }

        let app = Router::new()
            .route(
                "/network",
                post(|State(db): State<Shared>, h: HeaderMap, Json(body): Json<Value>| async move {
                    authed(&h)?;
                    let mut db = db.lock().unwrap();
                    let id = format!("8056c2e21c{:06x}", db.networks.len() + 1);
                    let net = json!({ "id": id, "config": body["config"] });
                    db.networks.insert(id, net.clone());
                    Ok::<_, StatusCode>(Json(net))
                }),
            )
            .route(
                "/network/{id}",
                get(|State(db): State<Shared>, h: HeaderMap, Path(id): Path<String>| async move {
                    authed(&h)?;
                    db.lock().unwrap().networks.get(&id).cloned().map(Json).ok_or(StatusCode::NOT_FOUND)
                })
                .delete(|State(db): State<Shared>, h: HeaderMap, Path(id): Path<String>| async move {
                    authed(&h)?;
                    db.lock().unwrap().networks.remove(&id).map(|_| Json(json!({}))).ok_or(StatusCode::NOT_FOUND)
                }),
            )
            .route(
                "/network/{id}/member",
                get(|State(db): State<Shared>, h: HeaderMap, Path(id): Path<String>| async move {
                    authed(&h)?;
                    let db = db.lock().unwrap();
                    let members: Vec<Value> =
                        db.members.iter().filter(|((n, _), _)| *n == id).map(|(_, m)| m.clone()).collect();
                    Ok::<_, StatusCode>(Json(members))
                }),
            )
            .route(
                "/network/{id}/member/{node}",
                post(
                    |State(db): State<Shared>,
                     h: HeaderMap,
                     Path((id, node)): Path<(String, String)>,
                     Json(body): Json<Value>| async move {
                        authed(&h)?;
                        let mut db = db.lock().unwrap();
                        if !db.networks.contains_key(&id) {
                            return Err(StatusCode::NOT_FOUND);
                        }
                        let m = db.members.entry((id, node.clone())).or_insert_with(|| {
                            json!({ "nodeId": node, "config": { "authorized": false, "ipAssignments": [] } })
                        });
                        if let Some(a) = body["config"]["authorized"].as_bool() {
                            m["config"]["authorized"] = json!(a);
                            m["config"]["ipAssignments"] = json!(if a { vec!["10.147.17.5"] } else { vec![] });
                        }
                        if let Some(n) = body["name"].as_str() {
                            m["name"] = json!(n);
                        }
                        Ok(Json(m.clone()))
                    },
                ),
            )
            .with_state(Shared::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    async fn zt_app() -> (tempfile::TempDir, App, Arc<FakeZt>) {
        let (dir, app, _) = app().await;
        let zt = Arc::new(FakeZt::default());
        let app = app.with_exec(zt.clone());
        let url = fake_central().await;
        app.profile_add(crate::AddProfile {
            kind: ProfileKind::Zerotier,
            name: "zt".into(),
            values: [("api_url", url.as_str()), ("api_token", "secret")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        })
        .await
        .unwrap();
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        (dir, app, zt)
    }

    #[tokio::test]
    async fn networks_are_created_linked_and_removed() {
        let (_dir, app, _) = zt_app().await;
        let n = app
            .zt_create("dev-net", "zt", None, Some("wl-acme-dev-001"))
            .await
            .unwrap();
        assert_eq!(n.cidr.as_deref(), Some("10.147.17.0/24"));
        assert_eq!(n.origin, NetworkOrigin::Created);
        assert_eq!(n.workloads, ["wl-acme-dev-001"]);
        let detail = app.zt_show("dev-net").await.unwrap();
        assert!(detail.central.private);
        assert_eq!(detail.central.name, "acme-dev-net");
        assert_eq!(detail.central.pools, ["10.147.17.1-10.147.17.254"]);

        // The next network gets the next free range.
        let other = app.zt_create("qa-net", "zt", None, None).await.unwrap();
        assert_eq!(other.cidr.as_deref(), Some("10.147.18.0/24"));
        assert!(app.zt_create("dev-net", "zt", None, None).await.is_err());
        assert!(app.zt_create("Bad Name", "zt", None, None).await.is_err());

        // Attaching checks the network exists in Central.
        let attached = app.zt_attach("shared", &other.network_id, "zt", None).await;
        assert!(attached.is_err(), "the same network ID twice is a conflict");
        assert!(
            app.zt_attach("ghost", "ffffffffffffffff", "zt", None)
                .await
                .is_err()
        );

        assert!(matches!(
            app.zt_remove("dev-net", true).await,
            Err(AppError::InUse { .. })
        ));
        app.zt_link("qa-net", "wl-acme-dev-001").await.unwrap();
        assert_eq!(
            app.zt_for_workload("wl-acme-dev-001")
                .await
                .unwrap()
                .unwrap()
                .name,
            "qa-net"
        );
        app.zt_remove("dev-net", true).await.unwrap();
        app.zt_unlink("wl-acme-dev-001").await.unwrap();
        assert!(app.zt_unlink("wl-acme-dev-001").await.is_err());
        app.zt_remove("qa-net", false).await.unwrap();
        assert!(app.zt_list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn join_authorizes_the_node() {
        let (_dir, app, zt) = zt_app().await;
        app.zt_create("dev-net", "zt", None, None).await.unwrap();
        let joined = app
            .zt_join("dev-net", Some("laptop"), true, None)
            .await
            .unwrap();
        assert_eq!(joined.node_id, "a1b2c3d4e5");
        let member = joined.member.unwrap();
        assert!(member.authorized);
        assert_eq!(member.name, "laptop");
        assert!(
            zt.0.lock()
                .unwrap()
                .iter()
                .any(|s| s.contains(&format!("'join' '{}'", joined.network_id))),
            "zerotier-cli join ran"
        );

        let members = app.zt_members("dev-net").await.unwrap();
        assert_eq!(members.len(), 1);
        let m = app
            .zt_authorize("dev-net", "a1b2c3d4e5", false, None)
            .await
            .unwrap();
        assert!(!m.authorized && m.ips.is_empty());
        assert!(
            app.zt_authorize("dev-net", "nope", true, None)
                .await
                .is_err()
        );

        assert_eq!(
            app.zt_leave("dev-net", None, true).await.unwrap(),
            "a1b2c3d4e5"
        );
    }

    #[tokio::test]
    async fn http_binds_only_loopback_or_zerotier() {
        let (_dir, app, _) = app().await;
        let app = app.with_exec(Arc::new(FakeZt::default()));
        let addr = |s: &'static str| {
            let app = &app;
            async move { app.mcp_http_addr(s).await }
        };
        assert_eq!(
            addr("localhost:7420").await.unwrap().to_string(),
            "127.0.0.1:7420"
        );
        assert_eq!(addr("[::1]:7420").await.unwrap().to_string(), "[::1]:7420");
        for zt in [
            "zt:7420",
            "8056c2e21c000001:7420",
            "ztabc:7420",
            "10.147.17.5:7420",
        ] {
            assert_eq!(
                addr(zt).await.unwrap().to_string(),
                "10.147.17.5:7420",
                "{zt}"
            );
        }
        for bad in [
            "0.0.0.0:7420",
            "192.168.1.4:7420",
            "eth0:7420",
            "localhost",
            "zt:http",
        ] {
            assert!(addr(bad).await.is_err(), "{bad}");
        }
    }
}
