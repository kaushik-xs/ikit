//! Routes: domains (and path prefixes) that the target's reverse proxy sends to a service
//! port, which is published on the host's loopback address at an upstream port ikit picks.
//! Redirects send a domain elsewhere. Automatic routes follow a workload's services.

use std::collections::{BTreeMap, BTreeSet};

use ikit_core::config::default_port_name;
use ikit_core::edge::{expand_host_template, is_hostname, parse_redirect_target, parse_route_path};
use ikit_core::{
    AutoRoutes, Category, PublishedPort, Redirect, Route, RouteOrigin, ServiceInstance,
    ServiceSpec, TlsMode, Workload,
};
use ikit_state::{NewRedirect, NewRoute};
use serde::Serialize;

use crate::{App, AppError, Result};

/// What `ikit route add` asks for.
#[derive(Debug, Clone, Default)]
pub struct RouteRequest {
    pub service: String,
    /// A container port number or a registry port name; default: the port named `http`,
    /// else the service's first TCP port.
    pub port: Option<String>,
    /// Default: the org's host template.
    pub host: Option<String>,
    pub path: Option<String>,
    /// Default: the org's `edge.default_tls`.
    pub tls: Option<TlsMode>,
}

/// A workload's routes, redirects and automatic-route settings.
#[derive(Debug, Clone, Serialize)]
pub struct RouteList {
    pub workload: String,
    pub target: String,
    pub routes: Vec<Route>,
    pub redirects: Vec<Redirect>,
    pub auto: Option<AutoRoutes>,
}

impl App {
    pub async fn route_list(&self, workload: &str) -> Result<RouteList> {
        let wl = self.workload_get(workload).await?;
        Ok(RouteList {
            routes: self.store.edge().routes(wl.id).await?,
            redirects: self.store.edge().redirects(wl.id).await?,
            auto: self.store.edge().auto_routes(wl.id).await?,
            workload: wl.name,
            target: wl.target,
        })
    }

    /// Routes a domain (and optional path prefix) to a service port. Takes effect on the
    /// next apply.
    pub async fn route_add(&self, workload: &str, req: RouteRequest) -> Result<Route> {
        let wl = self.workload_get(workload).await?;
        let services = self.store.workloads().services(wl.id).await?;
        let service = services
            .iter()
            .find(|s| s.name == req.service)
            .ok_or_else(|| {
                AppError::NotFound(format!("service `{}` in {workload}", req.service))
            })?;
        let spec = self.instance_spec(service).await?;
        let port = route_port(service, &spec, req.port.as_deref())?;
        let config = self.config()?;
        let host = match req.host {
            Some(h) => {
                let h = h.to_ascii_lowercase();
                if !is_hostname(&h) {
                    return Err(AppError::Invalid(format!(
                        "`{h}` is not a domain name (wildcards are not supported)"
                    )));
                }
                h
            }
            None => self.template_host(&wl, &service.name)?,
        };
        let path = req
            .path
            .as_deref()
            .map(parse_route_path)
            .transpose()
            .map_err(AppError::Invalid)?
            .flatten();
        let tls = req.tls.unwrap_or(config.edge.default_tls);
        self.check_host_free(&wl, &host, Some(tls)).await?;
        let upstream_port = self.upstream_port(&wl, &service.name, port).await?;
        let new = NewRoute {
            service: &service.name,
            port,
            host: &host,
            path: path.as_deref(),
            tls,
            upstream_port,
            origin: RouteOrigin::Manual,
        };
        // A manual route replaces the automatic one for the same host and path.
        let existing = self.store.edge().routes(wl.id).await?;
        if existing
            .iter()
            .any(|r| r.origin == RouteOrigin::Auto && r.host == host && r.path == path)
        {
            self.store
                .edge()
                .delete_route(wl.id, &host, path.as_deref())
                .await?;
        }
        Ok(self.store.edge().insert_route(wl.id, new).await?)
    }

    /// Removes a route by `host` or `host/path`. An automatic route is not added back.
    pub async fn route_remove(&self, workload: &str, key: &str) -> Result<Route> {
        let wl = self.workload_get(workload).await?;
        let routes = self.store.edge().routes(wl.id).await?;
        let route = routes
            .into_iter()
            .find(|r| r.key() == key.trim_end_matches('/'))
            .ok_or_else(|| AppError::NotFound(format!("route {key} in {workload}")))?;
        self.store
            .edge()
            .delete_route(wl.id, &route.host, route.path.as_deref())
            .await?;
        if route.origin == RouteOrigin::Auto
            && let Some(mut auto) = self.store.edge().auto_routes(wl.id).await?
            && !auto.excluded.contains(&route.service)
        {
            auto.excluded.push(route.service.clone());
            self.store
                .edge()
                .set_auto_routes(wl.id, Some(&auto))
                .await?;
        }
        Ok(route)
    }

    /// Redirects `host` to `to`, keeping the request path.
    pub async fn redirect_add(
        &self,
        workload: &str,
        host: &str,
        to: &str,
        permanent: bool,
        tls: Option<TlsMode>,
    ) -> Result<Redirect> {
        let wl = self.workload_get(workload).await?;
        let host = host.to_ascii_lowercase();
        if !is_hostname(&host) {
            return Err(AppError::Invalid(format!("`{host}` is not a domain name")));
        }
        let to = parse_redirect_target(to).map_err(AppError::Invalid)?;
        let tls = tls.unwrap_or(self.config()?.edge.default_tls);
        self.check_host_free(&wl, &host, None).await?;
        let new = NewRedirect {
            host: &host,
            to: &to,
            permanent,
            tls,
        };
        Ok(self.store.edge().insert_redirect(wl.id, new).await?)
    }

    pub async fn redirect_remove(&self, workload: &str, host: &str) -> Result<()> {
        let wl = self.workload_get(workload).await?;
        if !self.store.edge().delete_redirect(wl.id, host).await? {
            return Err(AppError::NotFound(format!("redirect {host} in {workload}")));
        }
        Ok(())
    }

    /// Turns automatic routes on (or changes their groups and port name) and adds them now.
    /// Needs `edge.base_domain` in the org config.
    pub async fn route_auto_on(
        &self,
        workload: &str,
        groups: Vec<Category>,
        port_name: Option<String>,
    ) -> Result<RouteList> {
        let wl = self.workload_get(workload).await?;
        if self.config()?.edge.base_domain.is_none() {
            return Err(AppError::Invalid(format!(
                "automatic routes need a base domain: set `edge.base_domain` in {}",
                self.org.config_file().display()
            )));
        }
        if groups.is_empty() {
            return Err(AppError::Invalid("pick at least one group".into()));
        }
        let excluded = self
            .store
            .edge()
            .auto_routes(wl.id)
            .await?
            .map(|a| a.excluded)
            .unwrap_or_default();
        let auto = AutoRoutes {
            groups,
            port_name: port_name.unwrap_or_else(default_port_name),
            excluded,
        };
        self.store
            .edge()
            .set_auto_routes(wl.id, Some(&auto))
            .await?;
        self.sync_auto_routes(workload).await?;
        self.route_list(workload).await
    }

    /// Turns automatic routes off and removes them; manual routes stay.
    pub async fn route_auto_off(&self, workload: &str) -> Result<RouteList> {
        let wl = self.workload_get(workload).await?;
        for r in self.store.edge().routes(wl.id).await? {
            if r.origin == RouteOrigin::Auto {
                self.store
                    .edge()
                    .delete_route(wl.id, &r.host, r.path.as_deref())
                    .await?;
            }
        }
        self.store.edge().set_auto_routes(wl.id, None).await?;
        self.route_list(workload).await
    }

    /// Brings automatic routes in step with the workload's services: one per service in
    /// the chosen groups with a port named `port_name`, unless it has a manual route on
    /// that port or its auto route was removed by hand. Returns the hosts it could not add.
    pub(crate) async fn sync_auto_routes(&self, workload: &str) -> Result<Vec<String>> {
        let wl = self.workload_get(workload).await?;
        let Some(auto) = self.store.edge().auto_routes(wl.id).await? else {
            return Ok(vec![]);
        };
        if self.config()?.edge.base_domain.is_none() {
            return Ok(vec![]);
        }
        let services = self.store.workloads().services(wl.id).await?;
        let mut wanted: BTreeMap<String, u16> = BTreeMap::new();
        for s in services
            .iter()
            .filter(|s| auto.groups.contains(&s.group) && !auto.excluded.contains(&s.name))
        {
            let spec = self.instance_spec(s).await?;
            if let Some(p) = spec
                .ports
                .iter()
                .find(|p| p.name.as_deref() == Some(auto.port_name.as_str()))
            {
                wanted.insert(s.name.clone(), p.container);
            }
        }
        let routes = self.store.edge().routes(wl.id).await?;
        for r in routes.iter().filter(|r| r.origin == RouteOrigin::Auto) {
            if wanted.get(&r.service) != Some(&r.port) {
                self.store
                    .edge()
                    .delete_route(wl.id, &r.host, r.path.as_deref())
                    .await?;
            }
        }
        let mut skipped = Vec::new();
        for (service, port) in wanted {
            if routes
                .iter()
                .any(|r| r.service == service && r.port == port)
            {
                continue;
            }
            let host = match self.template_host(&wl, &service) {
                Ok(h) => h,
                Err(e) => {
                    skipped.push(e.to_string());
                    continue;
                }
            };
            let tls = self.config()?.edge.default_tls;
            if let Err(e) = self.check_host_free(&wl, &host, Some(tls)).await {
                skipped.push(e.to_string());
                continue;
            }
            let upstream_port = self.upstream_port(&wl, &service, port).await?;
            let new = NewRoute {
                service: &service,
                port,
                host: &host,
                path: None,
                tls,
                upstream_port,
                origin: RouteOrigin::Auto,
            };
            if let Err(e) = self.store.edge().insert_route(wl.id, new).await {
                skipped.push(e.to_string());
            }
        }
        Ok(skipped)
    }

    /// Runs [`App::sync_auto_routes`] after a service change; problems only warn.
    pub(crate) async fn after_services_changed(&self, workload: &str) {
        match self.sync_auto_routes(workload).await {
            Ok(skipped) => {
                for s in skipped {
                    tracing::warn!("automatic route not added: {s}");
                }
            }
            Err(e) => tracing::warn!("updating automatic routes of {workload}: {e}"),
        }
    }

    /// Routes and redirects of a removed service go with it.
    pub(crate) async fn remove_service_routes(
        &self,
        wl: &Workload,
        names: &[String],
    ) -> Result<()> {
        for n in names {
            self.store.edge().delete_service_routes(wl.id, n).await?;
        }
        Ok(())
    }

    /// The URLs each service answers on, first route first.
    pub async fn route_urls(&self, workload: &str) -> Result<BTreeMap<String, Vec<String>>> {
        let wl = self.workload_get(workload).await?;
        let mut urls: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in self.store.edge().routes(wl.id).await? {
            urls.entry(r.service.clone()).or_default().push(r.url());
        }
        Ok(urls)
    }

    /// The loopback publishes routes need, per service.
    pub(crate) async fn route_publishes(
        &self,
        wl: &Workload,
    ) -> Result<BTreeMap<String, Vec<PublishedPort>>> {
        let mut out: BTreeMap<String, Vec<PublishedPort>> = BTreeMap::new();
        for r in self.store.edge().routes(wl.id).await? {
            let ports = out.entry(r.service.clone()).or_default();
            if !ports.iter().any(|p| p.host == r.upstream_port) {
                ports.push(PublishedPort {
                    bind: Some("127.0.0.1".into()),
                    host: r.upstream_port,
                    container: r.port,
                    udp: false,
                });
            }
        }
        Ok(out)
    }

    async fn instance_spec(&self, s: &ServiceInstance) -> Result<ServiceSpec> {
        let entry = self
            .store
            .registry()
            .get(&s.registry_service)
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("registry service `{}`", s.registry_service))
            })?;
        Ok(entry
            .versions
            .into_iter()
            .find(|v| v.id == s.version_id)
            .map(|v| v.spec)
            .unwrap_or_default())
    }

    /// The org's host template filled in for `service` of `wl`.
    fn template_host(&self, wl: &Workload, service: &str) -> Result<String> {
        let config = self.config()?;
        let base = config.edge.base_domain.as_deref().ok_or_else(|| {
            AppError::Invalid(format!(
                "pass --host, or set `edge.base_domain` in {} to derive one",
                self.org.config_file().display()
            ))
        })?;
        expand_host_template(
            &config.edge.host_template,
            &[
                ("service", service),
                ("workload", &wl.name),
                ("env", &wl.env),
                ("org", &self.org.name),
                ("base_domain", base),
            ],
        )
        .map_err(AppError::Invalid)
    }

    /// Refuses a host another workload of this org on the same target routes or redirects,
    /// one this workload redirects, or (for a route) one it routes with another TLS mode.
    /// Other orgs' hosts are checked against the target's site files at plan time.
    async fn check_host_free(&self, wl: &Workload, host: &str, tls: Option<TlsMode>) -> Result<()> {
        let workloads: BTreeMap<String, String> = self
            .workload_list(None)
            .await?
            .into_iter()
            .map(|w| (w.name, w.target))
            .collect();
        let same_target = |name: &str| workloads.get(name) == Some(&wl.target);
        for (owner, r) in self.store.edge().all_routes().await? {
            if r.host != host {
                continue;
            }
            if owner != wl.name && same_target(&owner) {
                return Err(AppError::InUse {
                    what: host.to_owned(),
                    by: vec![format!("a route of {owner} on `{}`", wl.target)],
                });
            }
            match tls {
                None if owner == wl.name => {
                    return Err(AppError::AlreadyExists(format!(
                        "route on {host} in {} (remove it before redirecting the domain)",
                        wl.name
                    )));
                }
                Some(t) if owner == wl.name && r.tls != t => {
                    return Err(AppError::Invalid(format!(
                        "{} uses tls {}; routes on one domain share a TLS mode",
                        r.key(),
                        r.tls
                    )));
                }
                _ => {}
            }
        }
        for (owner, r) in self.store.edge().all_redirects().await? {
            if r.host == host && (owner == wl.name || same_target(&owner)) {
                return Err(AppError::InUse {
                    what: host.to_owned(),
                    by: vec![format!("a redirect of {owner}")],
                });
            }
        }
        Ok(())
    }

    /// The loopback port for `service`'s container `port`: the one an existing route of it
    /// already uses, or the first free one in the org's range. Free means not published or
    /// routed by any of this org's workloads on the target, and not used by another org's
    /// site on the target's proxy (when the host can be asked).
    async fn upstream_port(&self, wl: &Workload, service: &str, port: u16) -> Result<u16> {
        let routes = self.store.edge().all_routes().await?;
        if let Some((_, r)) = routes
            .iter()
            .find(|(w, r)| *w == wl.name && r.service == service && r.port == port)
        {
            return Ok(r.upstream_port);
        }
        let mut taken: BTreeSet<u16> = BTreeSet::new();
        let workloads = self.workload_list(None).await?;
        for other in workloads.iter().filter(|w| w.target == wl.target) {
            for s in self.store.workloads().services(other.id).await? {
                taken.extend(s.publish.iter().map(|p| p.host));
            }
            taken.extend(
                routes
                    .iter()
                    .filter(|(w, _)| *w == other.name)
                    .map(|(_, r)| r.upstream_port),
            );
        }
        let target = self.target_show(&wl.target).await?;
        if target.edge.is_some()
            && let Ok(probe) = self.edge_probe(&target.name).await
        {
            for (_, header) in &probe.sites {
                if let Some(h) = header {
                    taken.extend(h.routes.iter().map(|r| r.upstream));
                }
            }
        }
        let [first, last] = self.config()?.edge.upstream_ports;
        (first..=last).find(|p| !taken.contains(p)).ok_or_else(|| {
            AppError::Invalid(format!(
                "no free upstream port in {first}-{last} on `{}`; widen `edge.upstream_ports`",
                wl.target
            ))
        })
    }
}

/// The container port a route goes to: a number, a registry port name, or by default the
/// port named `http`, else the first TCP port.
fn route_port(service: &ServiceInstance, spec: &ServiceSpec, port: Option<&str>) -> Result<u16> {
    let tcp = || {
        spec.ports
            .iter()
            .filter(|p| p.protocol == ikit_core::registry::Protocol::Tcp)
    };
    match port {
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 => Ok(n),
            _ => tcp()
                .find(|q| q.name.as_deref() == Some(p))
                .map(|q| q.container)
                .ok_or_else(|| {
                    AppError::Invalid(format!("`{}` has no TCP port named {p}", service.name))
                }),
        },
        None => tcp()
            .find(|q| q.name.as_deref() == Some("http"))
            .or_else(|| tcp().next())
            .map(|q| q.container)
            .ok_or_else(|| {
                AppError::Invalid(format!(
                    "`{}` declares no TCP ports; pass --port <container port>",
                    service.name
                ))
            }),
    }
}
