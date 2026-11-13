//! Host ports on a target: which are taken, by whom, and which ikit may hand out next.
//!
//! One place answers "is this host port free on that target?" for publishing, route upstreams
//! and `plan`. A port is taken when one of this org's workloads on the target publishes or
//! routes it, when another org's site on the target's proxy routes to it, or (with
//! `ports.probe`, the default) when the host shows something listening on it or Docker
//! publishes it for a container. Ports in the org's `ports.reserved` (and ikit's built-in
//! reservations) are never handed out.

use std::fmt;
use std::time::Duration;

use serde::Serialize;

use ikit_core::{Connection, PortDefaults, PortRange, PublishedPort, Workload, WorkloadStatus};
use ikit_provider::listeners::{self, ClaimedPort, Holder, OrgClaims};

use crate::edge::Root;
use crate::{App, AppError, Result};

const PROBE: Duration = Duration::from_secs(30);

/// Who holds a host port on a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Owner {
    /// A service of this org publishes it.
    Service { workload: String, service: String },
    /// This org's route upstream: the service's port on the host's loopback.
    Route { workload: String, service: String },
    /// Another org's route on the target's shared proxy.
    Site { org: String, workload: String },
    /// A process listening on the host; named when the probe could see it.
    Process(Option<String>),
    /// A container Docker publishes the port for, of any org or none.
    Container {
        project: Option<String>,
        name: String,
    },
    /// Another org's claim on the host: its workload publishes or routes to the port, whether
    /// or not it runs.
    OrgClaim {
        org: String,
        workload: String,
        service: String,
    },
}

impl Owner {
    /// This org's workload holding the port, if it is one.
    pub(crate) fn workload(&self) -> Option<&str> {
        match self {
            Owner::Service { workload, .. } | Owner::Route { workload, .. } => Some(workload),
            Owner::Site { .. }
            | Owner::Process(_)
            | Owner::Container { .. }
            | Owner::OrgClaim { .. } => None,
        }
    }
}

impl Owner {
    /// A short kind for tables and JSON.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Owner::Service { .. } => "service",
            Owner::Route { .. } => "route",
            Owner::Site { .. } => "org-route",
            Owner::Process(_) => "process",
            Owner::Container { .. } => "container",
            Owner::OrgClaim { .. } => "org-claim",
        }
    }

    /// Who holds the port, in a few words.
    pub(crate) fn label(&self) -> String {
        match self {
            Owner::Service { workload, service } => format!("{workload} / {service}"),
            Owner::Route { workload, service } => format!("{workload} / {service} (route)"),
            Owner::Site { org, workload } => format!("org {org}: {workload} (route)"),
            Owner::Process(Some(n)) => n.clone(),
            Owner::Process(None) => "unknown process".to_owned(),
            Owner::Container {
                project: Some(p),
                name,
            } => format!("{name} ({p})"),
            Owner::Container {
                project: None,
                name,
            } => name.clone(),
            Owner::OrgClaim {
                org,
                workload,
                service,
            } => format!("org {org}: {workload} / {service}"),
        }
    }
}

/// `ikit target ports`: every host port in use or claimed on a target, and what is still free.
#[derive(Debug, Clone, Serialize)]
pub struct PortReport {
    pub target: String,
    /// Host ports ikit hands out to services, e.g. `10000-19999`.
    pub range: String,
    /// TCP ports in `range` ikit could still hand out.
    pub free_in_range: usize,
    /// The port the next `svc publish <container port>` would get.
    pub next_free: Option<u16>,
    /// Loopback ports for route upstreams, e.g. `20000-29999`.
    pub upstream_range: String,
    /// Ports ikit never hands out, with why.
    pub reserved: Vec<String>,
    pub ports: Vec<PortEntry>,
    /// Why the host's listeners are not (fully) known, if they are not.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortEntry {
    pub port: u16,
    pub protocol: String,
    /// `None`: every address.
    pub bind: Option<String>,
    /// service, route, org-route, org-claim, process or container.
    pub kind: String,
    pub holder: String,
    /// This org's workload holding it, if one does.
    pub workload: Option<String>,
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Owner::Service { workload, service } => {
                write!(f, "published by `{service}` of {workload}")
            }
            Owner::Route { workload, service } => {
                write!(f, "the route upstream of `{service}` of {workload}")
            }
            Owner::Site { org, workload } => {
                write!(
                    f,
                    "a route upstream of org `{org}` ({workload}) on the proxy"
                )
            }
            Owner::Process(Some(name)) => write!(f, "in use by `{name}` on the host"),
            Owner::Process(None) => write!(
                f,
                "in use by a process on the host (ikit sees which one with root or passwordless sudo)"
            ),
            Owner::Container {
                project: Some(p),
                name,
            } => write!(f, "in use by container `{name}` (compose project `{p}`)"),
            Owner::Container {
                project: None,
                name,
            } => {
                write!(f, "in use by container `{name}`")
            }
            Owner::OrgClaim {
                org,
                workload,
                service,
            } => write!(f, "claimed by org `{org}` for `{service}` of {workload}"),
        }
    }
}

/// A host port someone holds on a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Claim {
    pub port: PublishedPort,
    pub owner: Owner,
}

impl Claim {
    /// The mapping, or only the address and port for something ikit did not publish.
    pub(crate) fn describe_port(&self) -> String {
        match self.owner {
            Owner::Process(_) | Owner::Container { .. } => {
                let p = &self.port;
                let bind = p.bind.as_deref().unwrap_or("0.0.0.0");
                let proto = if p.udp { "/udp" } else { "" };
                format!("{bind}:{}{proto}", p.host)
            }
            _ => self.port.to_string(),
        }
    }
}

/// Every claim on one target's host ports, plus the org's port policy.
#[derive(Debug, Clone)]
pub(crate) struct HostPorts {
    pub target: String,
    pub claims: Vec<Claim>,
    pub policy: PortDefaults,
    /// Why the host's listeners are not (fully) known, if they are not.
    pub notes: Vec<String>,
}

/// Which workloads count when collecting claims.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ClaimScope<'a> {
    /// Leave this workload out (`plan` compares a workload against the others).
    pub except: Option<&'a str>,
    /// Leave out drafts: they have no containers on the target yet.
    pub skip_drafts: bool,
    /// Ask the host what listens on it (when `ports.probe` is on and the target is a VM).
    pub probe: bool,
}

impl HostPorts {
    /// The claim `p` cannot be bound next to, if any.
    pub(crate) fn clash(&self, p: &PublishedPort) -> Option<&Claim> {
        self.claims.iter().find(|c| p.clashes(&c.port))
    }

    /// Whether ikit may hand out `port` (for TCP or UDP): nobody holds it on any address and
    /// it is not reserved.
    pub(crate) fn is_free(&self, port: u16, udp: bool) -> bool {
        self.policy.reserved_reason(port).is_none()
            && !self
                .claims
                .iter()
                .any(|c| c.port.host == port && c.port.udp == udp)
    }

    /// The first port in `range` that ikit may hand out.
    pub(crate) fn first_free(&self, range: PortRange, udp: bool) -> Option<u16> {
        (range.first..=range.last).find(|p| self.is_free(*p, udp))
    }
}

impl App {
    /// Collects the claims on `target`'s host ports. Other orgs' route upstreams come from the
    /// target's proxy when it has one and can be asked; a host that cannot be reached only
    /// leaves them out.
    pub(crate) async fn host_ports(
        &self,
        target: &str,
        scope: ClaimScope<'_>,
    ) -> Result<HostPorts> {
        let mut claims = Vec::new();
        // The excluded workload's own ports: its running containers hold them until apply.
        let mut own: Vec<PublishedPort> = Vec::new();
        let routes = self.store.edge().all_routes().await?;
        let workloads: Vec<Workload> = self.workload_list(None).await?;
        for wl in workloads.iter().filter(|w| w.target == target) {
            if scope.except == Some(wl.name.as_str()) {
                for s in self.store.workloads().services(wl.id).await? {
                    own.extend(s.publish);
                }
                own.extend(
                    routes
                        .iter()
                        .filter(|(w, _)| *w == wl.name)
                        .map(|(_, r)| loopback(r.upstream_port, r.port)),
                );
                continue;
            }
            if scope.skip_drafts && wl.status == WorkloadStatus::Draft {
                continue;
            }
            for s in self.store.workloads().services(wl.id).await? {
                claims.extend(s.publish.iter().map(|p| Claim {
                    port: p.clone(),
                    owner: Owner::Service {
                        workload: wl.name.clone(),
                        service: s.name.clone(),
                    },
                }));
            }
            for (_, r) in routes.iter().filter(|(w, _)| *w == wl.name) {
                claims.push(Claim {
                    port: loopback(r.upstream_port, r.port),
                    owner: Owner::Route {
                        workload: wl.name.clone(),
                        service: r.service.clone(),
                    },
                });
            }
        }
        let t = self.target_show(target).await?;
        if t.edge.is_some()
            && let Ok(probe) = self.edge_probe(&t.name).await
        {
            for header in probe.sites.iter().filter_map(|(_, h)| h.as_ref()) {
                if header.org == self.org.name {
                    continue;
                }
                claims.extend(header.routes.iter().map(|r| Claim {
                    port: loopback(r.upstream, r.upstream),
                    owner: Owner::Site {
                        org: header.org.clone(),
                        workload: header.workload.clone(),
                    },
                }));
            }
        }
        let policy = self.config()?.ports;
        let mut notes = Vec::new();
        if scope.probe && policy.probe && !matches!(t.connection, Connection::K8s { .. }) {
            match self.probe_listeners(&t).await {
                Ok(found) => {
                    for bad in &found.bad_claims {
                        notes.push(format!("unreadable port claims on `{target}`: {bad}"));
                    }
                    for org in found.claims.iter().filter(|c| c.org != self.org.name) {
                        claims.extend(org.ports.iter().map(|c| Claim {
                            port: c.port.clone(),
                            owner: Owner::OrgClaim {
                                org: org.org.clone(),
                                workload: c.workload.clone(),
                                service: c.service.clone(),
                            },
                        }));
                    }
                    if found.no_ss {
                        notes.push(format!(
                            "`ss` is not installed on `{target}`, so only container ports were checked; \
                             install iproute2 to see every listening port"
                        ));
                    }
                    for l in found.listeners {
                        let same = |p: &PublishedPort| p.host == l.port && p.udp == l.udp;
                        // The excluded workload's running containers, and sockets that may be
                        // their Docker proxy (when Docker itself could not be asked).
                        let mine = match &l.holder {
                            Holder::Container {
                                project: Some(p), ..
                            } => scope.except == Some(p.as_str()),
                            Holder::Process(None) => own.iter().any(same),
                            Holder::Process(Some(name)) => {
                                name == "docker-proxy" && own.iter().any(same)
                            }
                            Holder::Container { project: None, .. } => false,
                        };
                        if mine || claims.iter().any(|c| same(&c.port)) {
                            continue;
                        }
                        claims.push(Claim {
                            port: PublishedPort {
                                bind: l.bind,
                                host: l.port,
                                container: l.port,
                                udp: l.udp,
                            },
                            owner: match l.holder {
                                Holder::Process(name) => Owner::Process(name),
                                Holder::Container { project, name } => {
                                    Owner::Container { project, name }
                                }
                            },
                        });
                    }
                }
                Err(e) => notes.push(format!(
                    "could not check the listening ports on `{target}`: {e}"
                )),
            }
        }
        Ok(HostPorts {
            target: target.to_owned(),
            claims,
            policy,
            notes,
        })
    }

    /// Every host port in use or claimed on `target`: this org's services and routes (running
    /// or not), other orgs' claims and routes, and whatever the host shows listening.
    pub async fn target_ports(&self, target: &str) -> Result<PortReport> {
        let t = self.target_show(target).await?;
        if matches!(t.connection, Connection::K8s { .. }) {
            return Err(AppError::Invalid(format!(
                "`{target}` is a Kubernetes cluster; host ports apply to VM targets"
            )));
        }
        let scope = ClaimScope {
            probe: true,
            ..ClaimScope::default()
        };
        let ports = self.host_ports(target, scope).await?;
        let range = ports.policy.range();
        let [ufirst, ulast] = self.config()?.edge.upstream_ports;
        let mut reserved: Vec<String> = ports
            .policy
            .reserved
            .iter()
            .map(|r| format!("{r} (config.yaml)"))
            .collect();
        reserved.extend(
            ikit_core::ports::BUILTIN_RESERVED
                .iter()
                .map(|(r, why)| format!("{r} ({why})")),
        );
        let mut entries: Vec<PortEntry> = ports
            .claims
            .iter()
            .map(|c| PortEntry {
                port: c.port.host,
                protocol: if c.port.udp { "udp" } else { "tcp" }.to_owned(),
                bind: c.port.bind.clone(),
                kind: c.owner.kind().to_owned(),
                holder: c.owner.label(),
                workload: c.owner.workload().map(str::to_owned),
            })
            .collect();
        entries.sort_by(|a, b| (a.port, &a.protocol).cmp(&(b.port, &b.protocol)));
        entries.dedup();
        Ok(PortReport {
            target: target.to_owned(),
            range: range.to_string(),
            free_in_range: (range.first..=range.last)
                .filter(|p| ports.is_free(*p, false))
                .count(),
            next_free: ports.first_free(range, false),
            upstream_range: PortRange::new(ufirst, ulast).to_string(),
            reserved,
            ports: entries,
            notes: ports.notes,
        })
    }

    /// The workload's host ports that something other than its own containers holds on the
    /// target now (a host process or another container): `(service, detail)`. Ports it
    /// shares with this org's other workloads, or another org's claims, are config clashes
    /// that `plan` reports, not drift.
    pub(crate) async fn port_drift(&self, wl: &Workload) -> Result<Vec<(String, String)>> {
        let services = self.store.workloads().services(wl.id).await?;
        let routes = self.store.edge().routes(wl.id).await?;
        let mut mine: Vec<(String, PublishedPort)> = services
            .iter()
            .flat_map(|s| s.publish.iter().map(|p| (s.name.clone(), p.clone())))
            .collect();
        mine.extend(
            routes
                .iter()
                .map(|r| (r.service.clone(), loopback(r.upstream_port, r.port))),
        );
        if mine.is_empty() {
            return Ok(vec![]);
        }
        let scope = ClaimScope {
            except: Some(&wl.name),
            skip_drafts: false,
            probe: true,
        };
        let ports = self.host_ports(&wl.target, scope).await?;
        let mut drift = Vec::new();
        for (service, p) in &mine {
            if let Some(c) = ports.claims.iter().find(|c| {
                matches!(c.owner, Owner::Process(_) | Owner::Container { .. }) && p.clashes(&c.port)
            }) {
                drift.push((
                    service.clone(),
                    format!(
                        "host port {} ({p}) is {} ({}), not by this workload",
                        p.host,
                        c.owner,
                        c.describe_port()
                    ),
                ));
            }
        }
        for note in &ports.notes {
            tracing::warn!("{note}");
        }
        Ok(drift)
    }

    /// Records this org's host ports on `target` (every workload there, running or not) in
    /// the org's claim file on the host, so other orgs do not pick them. Problems only warn:
    /// the next change or apply writes the file again.
    pub(crate) async fn share_port_claims(&self, target: &str) {
        if let Err(e) = self.write_port_claims(target).await {
            tracing::warn!("could not record this org's ports on `{target}`: {e}");
        }
    }

    async fn write_port_claims(&self, target: &str) -> Result<()> {
        if !self.config()?.ports.probe {
            return Ok(());
        }
        let t = self.target_show(target).await?;
        if matches!(t.connection, Connection::K8s { .. }) {
            return Ok(());
        }
        let claims = self.org_claims(target).await?;
        let json = serde_json::to_string(&claims).expect("claims serialize");
        let script = listeners::write_claims_script(&self.org.name);
        let out = self
            .edge_sh(&t, &script, Root::IfAvailable, Some(json.as_bytes()), PROBE)
            .await?;
        if !out.success() {
            return Err(AppError::Invalid(out.failure_detail()));
        }
        Ok(())
    }

    /// This org's claim file for `target`.
    pub(crate) async fn org_claims(&self, target: &str) -> Result<OrgClaims> {
        let routes = self.store.edge().all_routes().await?;
        let mut ports = Vec::new();
        for wl in self.workload_list(None).await? {
            if wl.target != target {
                continue;
            }
            for s in self.store.workloads().services(wl.id).await? {
                ports.extend(s.publish.iter().map(|p| ClaimedPort {
                    workload: wl.name.clone(),
                    service: s.name.clone(),
                    port: p.clone(),
                }));
            }
            ports.extend(
                routes
                    .iter()
                    .filter(|(w, _)| *w == wl.name)
                    .map(|(_, r)| ClaimedPort {
                        workload: wl.name.clone(),
                        service: r.service.clone(),
                        port: loopback(r.upstream_port, r.port),
                    }),
            );
        }
        ports.dedup();
        let updated = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Ok(OrgClaims {
            org: self.org.name.clone(),
            updated,
            ports,
        })
    }

    /// Runs the listener probe on a VM target (as root when it can, to see process names).
    async fn probe_listeners(
        &self,
        target: &ikit_core::Target,
    ) -> Result<listeners::HostListeners> {
        let out = self
            .edge_sh(
                target,
                &listeners::probe_script(),
                Root::IfAvailable,
                None,
                PROBE,
            )
            .await?;
        if !out.success() {
            return Err(AppError::Invalid(out.failure_detail()));
        }
        Ok(listeners::parse_probe(&out.stdout))
    }

    /// Fails if `port` is in the org's `ports.reserved`. Built-in reservations only keep ikit
    /// from handing a port out; publishing one explicitly is allowed.
    pub(crate) fn check_not_reserved(&self, service: &str, port: &PublishedPort) -> Result<()> {
        match self.config()?.ports.reserved_by_org(port.host) {
            Some(r) => Err(AppError::Invalid(format!(
                "`{service}` cannot publish host port {}: it is reserved in config.yaml (ports.reserved: {r})",
                port.host
            ))),
            None => Ok(()),
        }
    }
}

fn loopback(host: u16, container: u16) -> PublishedPort {
    PublishedPort {
        bind: Some("127.0.0.1".to_owned()),
        host,
        container,
        udp: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports(claims: Vec<Claim>, reserved: Vec<PortRange>) -> HostPorts {
        HostPorts {
            target: "box".into(),
            claims,
            policy: PortDefaults {
                reserved,
                ..PortDefaults::default()
            },
            notes: vec![],
        }
    }

    fn svc(p: &str) -> Claim {
        Claim {
            port: p.parse().unwrap(),
            owner: Owner::Service {
                workload: "wl-a".into(),
                service: "web".into(),
            },
        }
    }

    #[test]
    fn skips_claimed_and_reserved_ports() {
        let hp = ports(
            vec![svc("30000:80"), svc("127.0.0.1:30001:80")],
            vec![PortRange::new(30002, 30003)],
        );
        assert_eq!(
            hp.first_free(PortRange::new(30000, 30010), false),
            Some(30004)
        );
        // UDP is a separate space.
        assert_eq!(
            hp.first_free(PortRange::new(30000, 30010), true),
            Some(30000)
        );
        assert_eq!(hp.first_free(PortRange::new(30000, 30003), false), None);
        // Built-in reservations apply even without config.
        assert_eq!(hp.first_free(PortRange::new(1020, 1030), false), Some(1024));
        assert_eq!(hp.first_free(PortRange::new(9993, 9994), false), Some(9994));
    }

    #[test]
    fn clashes_follow_bind_addresses() {
        let hp = ports(vec![svc("127.0.0.1:8080:80")], vec![]);
        assert!(hp.clash(&"8080:80".parse().unwrap()).is_some());
        assert!(hp.clash(&"10.0.0.5:8080:80".parse().unwrap()).is_none());
        assert!(hp.clash(&"8080:80/udp".parse().unwrap()).is_none());
        let c = hp.clash(&"127.0.0.1:8080:81".parse().unwrap()).unwrap();
        assert_eq!(c.owner.workload(), Some("wl-a"));
        assert_eq!(c.owner.to_string(), "published by `web` of wl-a");
    }
}
