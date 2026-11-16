//! Workload → `compose.yaml` (no secrets) + `secrets.compose.yaml` (secret env only).

use std::collections::BTreeMap;

use ikit_core::registry::EnvVar;
use ikit_core::resource::parse_reference;
use ikit_core::template::{Part, own_env_ref, parse_template, template_body};
use ikit_core::{Category, DockerNetwork, ResourceApi, ServiceInstance, ServiceSpec};
use serde::Serialize;

/// One service with everything needed to render it.
#[derive(Debug, Clone)]
pub struct ServiceInput {
    pub instance: ServiceInstance,
    /// The spec of the instance's resolved registry version.
    pub spec: ServiceSpec,
    /// Secret env values from the keychain.
    pub secrets: BTreeMap<String, String>,
    /// URLs the service answers on through the target's reverse proxy, first route first.
    pub urls: Vec<String>,
    /// Resources the service reads with `resource:<name>.<field>`, its own and shared ones,
    /// by their name in its spec.
    pub resources: BTreeMap<String, ResourceValues>,
}

/// What a service can read from one resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceValues {
    pub api: ResourceApi,
    /// Field → value. Fields without a value (`endpoint` on AWS itself) are left out, and
    /// env vars reading them are not set.
    pub fields: BTreeMap<String, String>,
}

/// Another workload this one is linked to: its app and platform services answer here as
/// `<workload>-<service>` on its link network.
#[derive(Debug, Clone)]
pub struct LinkedWorkload {
    pub workload: String,
    pub services: Vec<ServiceInput>,
}

/// The Docker network a workload offers to the workloads linked to it. ikit creates it on
/// the target before either side is applied, so it is `external` in both compose files.
pub fn link_network_name(workload: &str) -> String {
    format!("{workload}-link")
}

#[derive(Debug, Clone)]
pub struct WorkloadInput {
    pub org: String,
    pub workload: String,
    /// The workload's env (`dev`, `prod`, ...), for `{{env}}` in templates.
    pub env: String,
    pub services: Vec<ServiceInput>,
    /// Workloads whose services this one reaches by `service:` references.
    pub linked: Vec<LinkedWorkload>,
    /// Other workloads link to this one, so its app and platform services join its link
    /// network.
    pub offers_link: bool,
    /// Domains that must resolve to the target's host from inside containers (the
    /// workload's own and linked route hosts), so a service calling its public URL reaches
    /// the reverse proxy instead of itself.
    pub host_aliases: Vec<String>,
    /// The ZeroTier network a sidecar container joins, if the workload runs one.
    pub zerotier: Option<ZtSidecar>,
    /// Docker networks outside the workload that a group joins as `external: true`.
    pub external_networks: Vec<DockerNetwork>,
}

/// A ZeroTier container on the host network that joins the target to a network, so the
/// workload's published ports are reachable at the target's ZeroTier address.
#[derive(Debug, Clone)]
pub struct ZtSidecar {
    /// The 16-digit network ID.
    pub network_id: String,
}

/// The sidecar's compose service name; no service instance may use it.
pub const ZT_SIDECAR: &str = "ikit-zerotier";
/// The sidecar image.
pub const ZT_SIDECAR_IMAGE: &str = "zerotier/zerotier:1.14.2";
/// Where the sidecar keeps its node identity, in a named volume so it survives recreates.
const ZT_HOME: &str = "/var/lib/zerotier-one";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// Compose project name (the workload name).
    pub project: String,
    pub compose: String,
    /// Empty when no service has secrets.
    pub secrets: String,
    /// Distinct images, for registry logins.
    pub images: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("service `{service}` env var {var}: {message}")]
    Env {
        service: String,
        var: String,
        message: String,
    },
    #[error("workload `{0}` has no services to deploy (add some with `ikit sg add`)")]
    Empty(String),
    #[error("service name `{0}` is reserved for the ZeroTier sidecar; rename the service")]
    Reserved(String),
    #[error(
        "service `{0}` publishes host ports and runs {1} replicas; replicas would fight over the same port (unpublish it with `ikit svc publish`)"
    )]
    ReplicasPublished(String, u32),
}

#[derive(Serialize)]
struct ComposeFile {
    name: String,
    services: BTreeMap<String, ComposeService>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    volumes: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    networks: BTreeMap<String, ComposeNetwork>,
}

#[derive(Serialize)]
struct ComposeNetwork {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    external: bool,
}

#[derive(Serialize)]
struct ServiceNetwork {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
}

#[derive(Serialize)]
struct ComposeService {
    image: String,
    restart: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    network_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    networks: BTreeMap<String, ServiceNetwork>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cap_add: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    devices: Vec<&'static str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    environment: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ports: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    extra_hosts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shm_size: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    volumes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    healthcheck: Option<ComposeHealthcheck>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    depends_on: BTreeMap<String, DependsOn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deploy: Option<Deploy>,
    labels: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct ComposeHealthcheck {
    test: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interval: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retries: Option<u32>,
}

#[derive(Serialize)]
struct DependsOn {
    condition: &'static str,
}

#[derive(Serialize)]
struct Deploy {
    #[serde(skip_serializing_if = "Option::is_none")]
    replicas: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<DeployResources>,
}

#[derive(Serialize)]
struct DeployResources {
    limits: BTreeMap<&'static str, String>,
}

#[derive(Serialize)]
struct SecretsFile {
    services: BTreeMap<String, SecretsService>,
}

#[derive(Serialize)]
struct SecretsService {
    environment: BTreeMap<String, String>,
}

/// Compose interpolates `$` in compose files; `$$` is a literal `$`.
fn escape(value: &str) -> String {
    value.replace('$', "$$")
}

/// The registry host of an image reference (`docker.io` for Docker Hub images).
pub fn image_registry(image: &str) -> String {
    match image.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => {
            first.to_owned()
        }
        _ => "docker.io".to_owned(),
    }
}

/// The groups whose networks a service in `group` joins: its own, plus the ones below it
/// (app → platform → data), so data services never see app or platform traffic.
fn reachable_groups(group: Category) -> &'static [Category] {
    match group {
        Category::App => &[Category::App, Category::Platform, Category::Data],
        Category::Platform => &[Category::Platform, Category::Data],
        Category::Data => &[Category::Data],
    }
}

/// A value an env var resolves to, and whether it must go to the secrets file.
struct Resolved {
    value: String,
    secret: bool,
}

/// The value set directly on `s` for `var` (instance value, keychain, or registry default),
/// without following `from:` references.
fn direct_value(s: &ServiceInput, var: &EnvVar) -> Option<Resolved> {
    if let Some(v) = s.secrets.get(&var.name) {
        return Some(Resolved {
            value: v.clone(),
            secret: true,
        });
    }
    if let Some(v) = s.instance.env.get(&var.name) {
        return Some(Resolved {
            value: v.clone(),
            secret: var.secret,
        });
    }
    var.default.clone().map(|value| Resolved {
        value,
        secret: var.secret,
    })
}

/// Resolves `resource:<name>.<field>` from the service's own resources; `None` when the
/// field has no value for this backend.
fn resolve_resource(s: &ServiceInput, name: &str, field: &str) -> Result<Option<Resolved>, String> {
    let r = s.resources.get(name).ok_or_else(|| {
        format!(
            "resource `{name}` is not recorded for `{}`; check `ikit res list`",
            s.instance.name
        )
    })?;
    let secret = r.api.field(field).ok_or_else(|| {
        format!(
            "{} resource `{name}` has no field `{field}` (fields: {})",
            r.api,
            r.api
                .fields()
                .iter()
                .map(|(f, _)| *f)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    Ok(r.fields.get(field).map(|value| Resolved {
        value: value.clone(),
        secret,
    }))
}

/// A service a `service:` reference found: in this workload, or in a linked one.
struct Found<'a> {
    svc: &'a ServiceInput,
    /// The linked workload it runs in; `None` for this workload.
    linked: Option<&'a str>,
}

/// Finds `name` among `services`: an instance name, or a registry service with exactly one
/// instance.
fn find_service<'a>(
    name: &str,
    services: &'a [ServiceInput],
) -> Result<Option<&'a ServiceInput>, String> {
    if let Some(s) = services.iter().find(|s| s.instance.name == name) {
        return Ok(Some(s));
    }
    let matches: Vec<&ServiceInput> = services
        .iter()
        .filter(|s| s.instance.registry_service == name)
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        _ => Err(format!(
            "`{name}` matches several services; use an instance name"
        )),
    }
}

/// `name` in this workload first, then in the linked ones. `<workload>/<name>` picks a
/// linked workload explicitly.
fn find_ref<'a>(name: &str, input: &'a WorkloadInput) -> Result<Found<'a>, String> {
    if let Some((wl, svc)) = name.split_once('/') {
        let linked = input
            .linked
            .iter()
            .find(|l| l.workload == wl)
            .ok_or_else(|| {
                format!(
                    "`{wl}` is not linked to {} (see `ikit wl link`)",
                    input.workload
                )
            })?;
        let svc = find_service(svc, &linked.services)?
            .ok_or_else(|| format!("`{svc}` is not a service in {wl}"))?;
        return Ok(Found {
            svc,
            linked: Some(&linked.workload),
        });
    }
    if let Some(svc) = find_service(name, &input.services)? {
        return Ok(Found { svc, linked: None });
    }
    let mut hits = Vec::new();
    for l in &input.linked {
        if let Some(svc) = find_service(name, &l.services)? {
            hits.push(Found {
                svc,
                linked: Some(&l.workload),
            });
        }
    }
    match hits.len() {
        0 if input.linked.is_empty() => Err(format!("`{name}` is not a service in this workload")),
        0 => Err(format!(
            "`{name}` is not a service in this workload or the ones linked to it"
        )),
        1 => Ok(hits.remove(0)),
        _ => Err(format!(
            "`{name}` runs in several linked workloads ({}); use service:<workload>/{name}",
            hits.iter()
                .filter_map(|h| h.linked)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The groups that join link networks: data services stay private to their workload.
fn joins_links(group: Category) -> bool {
    group != Category::Data
}

/// Resolves `service:<name>.host | .port | .port.<port name> | .env.<VAR> | .url |
/// .internal_url`. `<name>` is a service instance, or a registry service if exactly one
/// instance uses it, in this workload or a linked one. A `.host` must be on a network that
/// a service in `from` joins.
fn resolve_ref(reference: &str, from: Category, input: &WorkloadInput) -> Result<Resolved, String> {
    let rest = reference.strip_prefix("service:").ok_or_else(|| {
        format!("unsupported reference `{reference}` (expected service:<name>.<field>)")
    })?;
    let (name, field) = rest
        .split_once('.')
        .ok_or_else(|| format!("reference `{reference}` has no field"))?;
    let found = find_ref(name, input)?;
    let target = found.svc;
    let plain = |value: String| {
        Ok(Resolved {
            value,
            secret: false,
        })
    };
    let host = || -> Result<String, String> {
        let group = target.instance.group;
        match found.linked {
            None => {
                if !reachable_groups(from).contains(&group) {
                    return Err(format!(
                        "`{}` is in the {group} group, which a {from} service cannot reach",
                        target.instance.name
                    ));
                }
                Ok(target.instance.name.clone())
            }
            Some(wl) => {
                if !joins_links(group) {
                    return Err(format!(
                        "`{}` is a {group} service of {wl}; data services are not shared with linked workloads",
                        target.instance.name
                    ));
                }
                if !joins_links(from) {
                    return Err(format!(
                        "a {from} service cannot reach the linked workload {wl}"
                    ));
                }
                Ok(format!("{wl}-{}", target.instance.name))
            }
        }
    };
    let first_port = || match target.spec.ports.first() {
        Some(p) => Ok(p.container.to_string()),
        None => Err(format!("`{}` exposes no ports", target.instance.name)),
    };
    match field.split_once('.') {
        None if field == "host" => plain(host()?),
        None if field == "port" => plain(first_port()?),
        None if field == "internal_url" => plain(format!("http://{}:{}", host()?, first_port()?)),
        Some(("port", port_name)) => match target
            .spec
            .ports
            .iter()
            .find(|p| p.name.as_deref() == Some(port_name))
        {
            Some(p) => plain(p.container.to_string()),
            None => Err(format!(
                "`{}` has no port named {port_name}",
                target.instance.name
            )),
        },
        None if field == "url" => match target.urls.first() {
            Some(url) => plain(url.clone()),
            None => Err(format!(
                "`{}` has no route; add one with `ikit route add {}`",
                target.instance.name, target.instance.name
            )),
        },
        Some(("env", var)) => {
            let spec_var = target
                .spec
                .env_var(var)
                .ok_or_else(|| format!("`{}` has no env var {var}", target.instance.name))?;
            direct_value(target, spec_var)
                .ok_or_else(|| format!("`{}` env var {var} has no value", target.instance.name))
        }
        _ => Err(format!(
            "unknown field `{field}` in `{reference}` (use host, port, port.<name>, env.<VAR>, url or internal_url)"
        )),
    }
}

/// How deep `env.<VAR>` placeholders may chain before ikit calls it a loop.
const MAX_TEMPLATE_DEPTH: usize = 8;

/// The value of `var` on `s`: set on the instance, or from its `from:` reference or
/// template, or the registry default. `Ok(None)` when it has none.
fn resolve_var(
    s: &ServiceInput,
    var: &EnvVar,
    input: &WorkloadInput,
    depth: usize,
) -> Result<Option<Resolved>, String> {
    let inst = &s.instance;
    let Some(reference) = &var.from else {
        return Ok(direct_value(s, var));
    };
    // A value set on the instance wins over the reference.
    if inst.env.contains_key(&var.name) || s.secrets.contains_key(&var.name) {
        return Ok(direct_value(s, var));
    }
    if let Some(body) = template_body(reference) {
        return resolve_template(body, s, input, depth);
    }
    match parse_reference(reference) {
        Some(parsed) => {
            let (name, field) = parsed?;
            resolve_resource(s, name, field)
        }
        None => resolve_ref(reference, inst.group, input).map(Some),
    }
}

/// Fills a template's placeholders. A placeholder without a value leaves the whole value
/// unset; the result is secret if any part is.
fn resolve_template(
    body: &str,
    s: &ServiceInput,
    input: &WorkloadInput,
    depth: usize,
) -> Result<Option<Resolved>, String> {
    if depth > MAX_TEMPLATE_DEPTH {
        return Err("env.<VAR> placeholders loop".to_owned());
    }
    let mut value = String::new();
    let mut secret = false;
    for part in parse_template(body)? {
        let r = match part {
            Part::Text(t) => {
                value.push_str(t);
                continue;
            }
            Part::Ref(r) => r,
        };
        let resolved = match r {
            "org" => Some(input.org.clone().into()),
            "workload" => Some(input.workload.clone().into()),
            "env" => Some(input.env.clone().into()),
            "service" => Some(s.instance.name.clone().into()),
            _ => {
                if let Some(name) = own_env_ref(r) {
                    let var = s
                        .spec
                        .env_var(name)
                        .ok_or_else(|| format!("`{}` has no env var {name}", s.instance.name))?;
                    resolve_var(s, var, input, depth + 1)?
                } else if let Some(parsed) = parse_reference(r) {
                    let (name, field) = parsed?;
                    resolve_resource(s, name, field)?
                } else {
                    Some(resolve_ref(r, s.instance.group, input)?)
                }
            }
        };
        let Some(resolved) = resolved else {
            return Ok(None);
        };
        secret |= resolved.secret;
        value.push_str(&resolved.value);
    }
    Ok(Some(Resolved { value, secret }))
}

impl From<String> for Resolved {
    fn from(value: String) -> Self {
        Resolved {
            value,
            secret: false,
        }
    }
}

pub fn render(input: &WorkloadInput) -> Result<Rendered, RenderError> {
    if input.services.is_empty() {
        return Err(RenderError::Empty(input.workload.clone()));
    }
    let mut services = BTreeMap::new();
    let mut secret_services = BTreeMap::new();
    let mut volumes = BTreeMap::new();
    let mut images = Vec::new();
    // One network per group in use; Compose names them `<workload>_<group>`.
    let groups: Vec<Category> = Category::ALL
        .iter()
        .copied()
        .filter(|g| input.services.iter().any(|s| s.instance.group == *g))
        .collect();
    let mut networks: BTreeMap<String, ComposeNetwork> = groups
        .iter()
        .map(|g| {
            let net = ComposeNetwork {
                name: None,
                external: false,
            };
            (g.to_string(), net)
        })
        .collect();
    // Compose fails on a missing external network even if no service joins it.
    let external_networks: Vec<&DockerNetwork> = input
        .external_networks
        .iter()
        .filter(|e| groups.contains(&e.group))
        .collect();
    for ext in &external_networks {
        networks.insert(
            ext.name.clone(),
            ComposeNetwork {
                name: Some(ext.name.clone()),
                external: true,
            },
        );
    }
    // Link networks: the one this workload offers, and the ones of workloads it links to.
    // Only app and platform services join them.
    let mut link_networks: Vec<String> = Vec::new();
    if input.offers_link {
        link_networks.push(link_network_name(&input.workload));
    }
    link_networks.extend(input.linked.iter().map(|l| link_network_name(&l.workload)));
    let any_linking = groups.iter().any(|g| joins_links(*g));
    if any_linking {
        for name in &link_networks {
            networks.insert(
                name.clone(),
                ComposeNetwork {
                    name: Some(name.clone()),
                    external: true,
                },
            );
        }
    }

    for s in &input.services {
        let inst = &s.instance;
        let mut environment = BTreeMap::new();
        let mut secret_env = BTreeMap::new();
        for var in &s.spec.env {
            let resolved = resolve_var(s, var, input, 0).map_err(|message| RenderError::Env {
                service: inst.name.clone(),
                var: var.name.clone(),
                message,
            })?;
            match resolved {
                Some(r) if r.secret || var.secret => {
                    secret_env.insert(var.name.clone(), escape(&r.value))
                }
                Some(r) => environment.insert(var.name.clone(), escape(&r.value)),
                None if var.generate.is_some() => {
                    return Err(RenderError::Env {
                        service: inst.name.clone(),
                        var: var.name.clone(),
                        message: format!(
                            "generated secret is missing (create it with `ikit svc set {} --generate {}`)",
                            inst.name, var.name
                        ),
                    });
                }
                None if var.required => {
                    return Err(RenderError::Env {
                        service: inst.name.clone(),
                        var: var.name.clone(),
                        message: "required but has no value (set it with `ikit svc set`)"
                            .to_owned(),
                    });
                }
                None => None,
            };
        }

        let ports: Vec<String> = inst
            .publish
            .iter()
            .map(|p| {
                let bind = p.bind.as_ref().map(|b| format!("{b}:")).unwrap_or_default();
                let proto = if p.udp { "udp" } else { "tcp" };
                format!("{bind}{}:{}/{proto}", p.host, p.container)
            })
            .collect();

        let mut mounts = Vec::new();
        for v in &s.spec.volumes {
            let key = format!("{}_{}", inst.name, v.name);
            mounts.push(format!("{key}:{}", v.path));
            volumes.insert(key, BTreeMap::new());
        }

        let mut depends_on = BTreeMap::new();
        for dep in &s.spec.depends_on {
            let provider = input.services.iter().find(|o| {
                o.instance.registry_service == dep.name
                    && dep.constraint.matches(&o.instance.version)
            });
            // Unmet dependencies live outside the workload.
            if let Some(p) = provider {
                let condition = if p.spec.healthcheck.is_some() {
                    "service_healthy"
                } else {
                    "service_started"
                };
                depends_on.insert(p.instance.name.clone(), DependsOn { condition });
            }
        }

        let healthcheck = s.spec.healthcheck.as_ref().map(|h| ComposeHealthcheck {
            test: std::iter::once("CMD".to_owned())
                .chain(h.command.iter().cloned())
                .collect(),
            interval: h.interval.clone(),
            timeout: h.timeout.clone(),
            retries: h.retries,
        });

        let resources = s.spec.resources.as_ref().and_then(|r| {
            let mut limits = BTreeMap::new();
            if let Some(cpu) = &r.cpu {
                limits.insert("cpus", cpu.clone());
            }
            if let Some(mem) = &r.memory {
                limits.insert("memory", mem.clone());
            }
            (!limits.is_empty()).then_some(DeployResources { limits })
        });
        // One container is compose's default; only write replicas when there are more.
        let replicas = s.spec.replicas.filter(|&n| n > 1);
        if let Some(n) = replicas
            && !ports.is_empty()
        {
            return Err(RenderError::ReplicasPublished(inst.name.clone(), n));
        }
        let deploy = (replicas.is_some() || resources.is_some()).then_some(Deploy {
            replicas,
            resources,
        });

        let mut service_networks: BTreeMap<String, ServiceNetwork> = reachable_groups(inst.group)
            .iter()
            .filter(|g| groups.contains(g))
            .map(|g| (g.to_string(), ServiceNetwork { aliases: vec![] }))
            .collect();
        for ext in external_networks.iter().filter(|e| e.group == inst.group) {
            // Other workloads on a shared network may run a service of the same name;
            // the prefixed alias always reaches this one.
            let alias = format!("{}-{}", input.workload, inst.name);
            service_networks.insert(
                ext.name.clone(),
                ServiceNetwork {
                    aliases: vec![alias],
                },
            );
        }
        if joins_links(inst.group) {
            for name in &link_networks {
                // Linked workloads reach this service as `<workload>-<service>`, which is
                // unique on the target however many workloads share the network.
                let aliases = if input.offers_link && *name == link_network_name(&input.workload) {
                    vec![format!("{}-{}", input.workload, inst.name)]
                } else {
                    vec![]
                };
                service_networks.insert(name.clone(), ServiceNetwork { aliases });
            }
        }
        let extra_hosts: Vec<String> = if inst.group == Category::Data {
            vec![]
        } else {
            input
                .host_aliases
                .iter()
                .map(|h| format!("{h}:host-gateway"))
                .collect()
        };

        let labels = BTreeMap::from([
            ("ikit.org".to_owned(), input.org.clone()),
            ("ikit.workload".to_owned(), input.workload.clone()),
            ("ikit.service".to_owned(), inst.name.clone()),
            (
                "ikit.registry".to_owned(),
                format!("{}@{}", inst.registry_service, inst.version),
            ),
        ]);

        if !images.contains(&inst.image) {
            images.push(inst.image.clone());
        }
        if !secret_env.is_empty() {
            secret_services.insert(
                inst.name.clone(),
                SecretsService {
                    environment: secret_env,
                },
            );
        }
        services.insert(
            inst.name.clone(),
            ComposeService {
                image: match &inst.digest {
                    Some(d) if !inst.image.contains('@') => format!("{}@{d}", inst.image),
                    _ => inst.image.clone(),
                },
                restart: "unless-stopped",
                command: s.spec.command.iter().map(|c| escape(c)).collect(),
                network_mode: None,
                networks: service_networks,
                cap_add: vec![],
                devices: vec![],
                environment,
                ports,
                extra_hosts,
                shm_size: s.spec.shm_size.clone(),
                volumes: mounts,
                healthcheck,
                depends_on,
                deploy,
                labels,
            },
        );
    }

    if let Some(zt) = &input.zerotier {
        if services.contains_key(ZT_SIDECAR) {
            return Err(RenderError::Reserved(ZT_SIDECAR.to_owned()));
        }
        volumes.insert(ZT_SIDECAR.to_owned(), BTreeMap::new());
        images.push(ZT_SIDECAR_IMAGE.to_owned());
        services.insert(
            ZT_SIDECAR.to_owned(),
            ComposeService {
                image: ZT_SIDECAR_IMAGE.to_owned(),
                restart: "unless-stopped",
                // The image's entrypoint joins the networks it is given.
                command: vec![zt.network_id.clone()],
                network_mode: Some("host"),
                networks: BTreeMap::new(),
                cap_add: vec!["NET_ADMIN", "SYS_ADMIN"],
                devices: vec!["/dev/net/tun"],
                environment: BTreeMap::new(),
                ports: vec![],
                extra_hosts: vec![],
                shm_size: None,
                volumes: vec![format!("{ZT_SIDECAR}:{ZT_HOME}")],
                healthcheck: None,
                depends_on: BTreeMap::new(),
                deploy: None,
                labels: BTreeMap::from([
                    ("ikit.org".to_owned(), input.org.clone()),
                    ("ikit.workload".to_owned(), input.workload.clone()),
                    ("ikit.service".to_owned(), ZT_SIDECAR.to_owned()),
                ]),
            },
        );
    }

    let header = format!(
        "# Generated by ikit for {}/{}. Do not edit; changes are overwritten.\n",
        input.org, input.workload
    );
    let compose = ComposeFile {
        name: input.workload.clone(),
        services,
        volumes,
        networks,
    };
    let compose = format!(
        "{header}{}",
        serde_yaml_ng::to_string(&compose).expect("compose serializes")
    );
    let secrets = if secret_services.is_empty() {
        String::new()
    } else {
        let file = SecretsFile {
            services: secret_services,
        };
        format!(
            "{header}{}",
            serde_yaml_ng::to_string(&file).expect("secrets serialize")
        )
    };
    Ok(Rendered {
        project: input.workload.clone(),
        compose,
        secrets,
        images,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use ikit_core::registry::{EnvVar, Healthcheck, Port, Protocol, Resources, Volume};
    use ikit_core::{Category, PublishedPort, ServiceInstance, ServiceSpec};
    use semver::Version;

    use super::*;

    pub fn instance(name: &str, registry: &str, version: &str, image: &str) -> ServiceInstance {
        ServiceInstance {
            id: 1,
            name: name.into(),
            group: Category::Data,
            registry_service: registry.into(),
            constraint: "latest".parse().unwrap(),
            version_id: 1,
            version: Version::parse(version).unwrap(),
            image: image.into(),
            digest: None,
            deprecated: false,
            env: BTreeMap::new(),
            secret_env: vec![],
            publish: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    pub fn sample() -> WorkloadInput {
        let pg_spec = ServiceSpec {
            ports: vec![Port {
                container: 5432,
                name: Some("postgres".into()),
                protocol: Protocol::Tcp,
            }],
            env: vec![
                EnvVar {
                    name: "POSTGRES_USER".into(),
                    default: Some("postgres".into()),
                    ..Default::default()
                },
                EnvVar {
                    name: "POSTGRES_PASSWORD".into(),
                    required: true,
                    secret: true,
                    ..Default::default()
                },
            ],
            volumes: vec![Volume {
                name: "data".into(),
                path: "/var/lib/postgresql/data".into(),
            }],
            healthcheck: Some(Healthcheck {
                command: vec!["pg_isready".into(), "-U".into(), "postgres".into()],
                interval: Some("10s".into()),
                timeout: None,
                retries: Some(5),
            }),
            ..Default::default()
        };
        let mut pg = instance("postgres", "postgres", "16.6.0", "postgres:16.6");
        pg.secret_env = vec!["POSTGRES_PASSWORD".into()];

        let api_spec = ServiceSpec {
            env: vec![
                EnvVar {
                    name: "LOG_LEVEL".into(),
                    default: Some("info".into()),
                    ..Default::default()
                },
                EnvVar {
                    name: "DB_HOST".into(),
                    from: Some("service:postgres.host".into()),
                    ..Default::default()
                },
                EnvVar {
                    name: "DB_PORT".into(),
                    from: Some("service:postgres.port.postgres".into()),
                    ..Default::default()
                },
                EnvVar {
                    name: "DB_PASSWORD".into(),
                    from: Some("service:postgres.env.POSTGRES_PASSWORD".into()),
                    ..Default::default()
                },
            ],
            resources: Some(Resources {
                cpu: Some("0.5".into()),
                memory: Some("256M".into()),
            }),
            depends_on: vec!["postgres@^16".parse().unwrap()],
            ..Default::default()
        };
        let mut api = instance(
            "orders-api",
            "orders-api",
            "1.4.2",
            "ghcr.io/acme/orders-api:1.4.2",
        );
        api.group = Category::App;
        api.env.insert("LOG_LEVEL".into(), "debug".into());
        api.publish = vec!["8080:8080".parse::<PublishedPort>().unwrap()];

        WorkloadInput {
            org: "acme".into(),
            workload: "wl-acme-dev-001".into(),
            services: vec![
                ServiceInput {
                    instance: pg,
                    spec: pg_spec,
                    secrets: BTreeMap::from([("POSTGRES_PASSWORD".into(), "pa$$word".into())]),
                    urls: vec![],
                    resources: BTreeMap::new(),
                },
                ServiceInput {
                    instance: api,
                    spec: api_spec,
                    secrets: BTreeMap::new(),
                    urls: vec![],
                    resources: BTreeMap::new(),
                },
            ],
            zerotier: None,
            external_networks: vec![],
            env: "dev".into(),
            linked: vec![],
            offers_link: false,
            host_aliases: vec![],
        }
    }

    fn yaml(s: &str) -> serde_yaml_ng::Value {
        serde_yaml_ng::from_str(s).unwrap()
    }

    #[test]
    fn renders_services_volumes_and_dependencies() {
        let r = render(&sample()).unwrap();
        let c = yaml(&r.compose);
        assert_eq!(c["name"], "wl-acme-dev-001");
        let pg = &c["services"]["postgres"];
        assert_eq!(pg["image"], "postgres:16.6");
        assert_eq!(pg["environment"]["POSTGRES_USER"], "postgres");
        assert!(
            pg["environment"].get("POSTGRES_PASSWORD").is_none(),
            "secret kept out of compose.yaml"
        );
        assert_eq!(pg["volumes"][0], "postgres_data:/var/lib/postgresql/data");
        assert_eq!(pg["healthcheck"]["test"][0], "CMD");
        assert!(c["volumes"].get("postgres_data").is_some());

        let api = &c["services"]["orders-api"];
        assert_eq!(api["environment"]["LOG_LEVEL"], "debug");
        assert_eq!(api["environment"]["DB_HOST"], "postgres");
        assert_eq!(api["environment"]["DB_PORT"], "5432");
        assert_eq!(api["ports"][0], "8080:8080/tcp");
        assert_eq!(
            api["depends_on"]["postgres"]["condition"],
            "service_healthy"
        );
        assert_eq!(api["deploy"]["resources"]["limits"]["memory"], "256M");
        assert_eq!(api["labels"]["ikit.registry"], "orders-api@1.4.2");
        assert_eq!(r.images, ["postgres:16.6", "ghcr.io/acme/orders-api:1.4.2"]);
    }

    #[test]
    fn command_and_replicas_render_for_workers() {
        let mut input = sample();
        let pg = &mut input.services[0];
        pg.spec.command = vec!["postgres".into(), "-c".into(), "max_connections=$N".into()];
        pg.spec.replicas = Some(3);
        let c = yaml(&render(&input).unwrap().compose);
        let pg = &c["services"]["postgres"];
        assert_eq!(pg["command"][0], "postgres");
        assert_eq!(pg["command"][2], "max_connections=$$N");
        assert_eq!(pg["deploy"]["replicas"], 3);
        assert!(pg["deploy"].get("resources").is_none());
        let api = &c["services"]["orders-api"];
        assert!(api.get("command").is_none());
        assert!(api["deploy"].get("replicas").is_none());
    }

    #[test]
    fn one_replica_is_left_to_compose() {
        let mut input = sample();
        input.services[0].spec.replicas = Some(1);
        let c = yaml(&render(&input).unwrap().compose);
        assert!(c["services"]["postgres"].get("deploy").is_none());
    }

    #[test]
    fn replicas_with_published_ports_are_refused() {
        let mut input = sample();
        input.services[1].spec.replicas = Some(2);
        assert_eq!(
            render(&input).unwrap_err(),
            RenderError::ReplicasPublished("orders-api".into(), 2)
        );
    }

    #[test]
    fn secrets_go_to_their_own_file_escaped() {
        let r = render(&sample()).unwrap();
        let s = yaml(&r.secrets);
        assert_eq!(
            s["services"]["postgres"]["environment"]["POSTGRES_PASSWORD"],
            "pa$$$$word"
        );
        // A reference to a secret stays secret.
        assert_eq!(
            s["services"]["orders-api"]["environment"]["DB_PASSWORD"],
            "pa$$$$word"
        );
        assert!(!r.compose.contains("pa$$"), "{}", r.compose);
    }

    #[test]
    fn missing_required_values_and_bad_references_fail() {
        let mut input = sample();
        input.services[0].secrets.clear();
        let err = render(&input).unwrap_err();
        assert!(err.to_string().contains("POSTGRES_PASSWORD"), "{err}");

        let mut input = sample();
        input.services[1].spec.env[1].from = Some("service:redis.host".into());
        assert!(render(&input).unwrap_err().to_string().contains("redis"));
    }

    #[test]
    fn pinned_images_use_their_digest() {
        let mut input = sample();
        input.services[0].instance.digest = Some("sha256:abc".into());
        let c = yaml(&render(&input).unwrap().compose);
        assert_eq!(
            c["services"]["postgres"]["image"],
            "postgres:16.6@sha256:abc"
        );
    }

    #[test]
    fn zerotier_sidecar_joins_on_the_host_network() {
        let mut input = sample();
        input.zerotier = Some(ZtSidecar {
            network_id: "8056c2e21c000001".into(),
        });
        let r = render(&input).unwrap();
        let c = yaml(&r.compose);
        let zt = &c["services"][ZT_SIDECAR];
        assert_eq!(zt["image"], ZT_SIDECAR_IMAGE);
        assert_eq!(zt["command"][0], "8056c2e21c000001");
        assert_eq!(zt["network_mode"], "host");
        assert_eq!(zt["devices"][0], "/dev/net/tun");
        assert_eq!(zt["volumes"][0], "ikit-zerotier:/var/lib/zerotier-one");
        assert!(c["volumes"].get(ZT_SIDECAR).is_some());
        assert!(r.images.contains(&ZT_SIDECAR_IMAGE.to_owned()));
        // Other services are unchanged.
        assert!(c["services"]["postgres"].get("network_mode").is_none());

        input.services[0].instance.name = ZT_SIDECAR.into();
        assert_eq!(
            render(&input).unwrap_err(),
            RenderError::Reserved(ZT_SIDECAR.into())
        );
    }

    #[test]
    fn groups_get_their_own_networks() {
        let c = yaml(&render(&sample()).unwrap().compose);
        // postgres is in data, orders-api in app; there is no platform service.
        assert_eq!(c["networks"]["app"], yaml("{}"));
        assert_eq!(c["networks"]["data"], yaml("{}"));
        assert!(c["networks"].get("platform").is_none());
        let joined = |svc: &str| -> Vec<String> {
            c["services"][svc]["networks"]
                .as_mapping()
                .unwrap()
                .keys()
                .map(|k| k.as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            joined("postgres"),
            ["data"],
            "data stays off the app network"
        );
        assert_eq!(joined("orders-api"), ["app", "data"]);
    }

    #[test]
    fn host_references_must_be_reachable_over_the_group_networks() {
        let mut input = sample();
        input.services[0].spec.env.push(EnvVar {
            name: "API_HOST".into(),
            from: Some("service:orders-api.host".into()),
            ..Default::default()
        });
        let err = render(&input).unwrap_err().to_string();
        assert!(err.contains("API_HOST"), "{err}");
        assert!(err.contains("cannot reach"), "{err}");

        // Ports and env values are plain values, so they may point up a group.
        let mut input = sample();
        input.services[1].spec.ports = vec![Port {
            container: 8080,
            name: None,
            protocol: Protocol::Tcp,
        }];
        input.services[0].spec.env.push(EnvVar {
            name: "API_PORT".into(),
            from: Some("service:orders-api.port".into()),
            ..Default::default()
        });
        let c = yaml(&render(&input).unwrap().compose);
        assert_eq!(c["services"]["postgres"]["environment"]["API_PORT"], "8080");

        // A URL comes from the service's first route.
        input.services[1].spec.env.push(EnvVar {
            name: "PUBLIC_URL".into(),
            from: Some("service:orders-api.url".into()),
            ..Default::default()
        });
        let err = render(&input).unwrap_err().to_string();
        assert!(err.contains("has no route"), "{err}");
        input.services[1].urls = vec!["https://api.acme.com".into()];
        let c = yaml(&render(&input).unwrap().compose);
        assert_eq!(
            c["services"]["orders-api"]["environment"]["PUBLIC_URL"],
            "https://api.acme.com"
        );
    }

    #[test]
    fn external_networks_are_joined_by_their_group_with_an_alias() {
        let mut input = sample();
        input.zerotier = Some(ZtSidecar {
            network_id: "8056c2e21c000001".into(),
        });
        input.external_networks = vec![DockerNetwork {
            name: "regere-dev-platform-network".into(),
            group: Category::App,
        }];
        let c = yaml(&render(&input).unwrap().compose);
        let net = &c["networks"]["regere-dev-platform-network"];
        assert_eq!(net["name"], "regere-dev-platform-network");
        assert_eq!(net["external"], true);
        let api = &c["services"]["orders-api"]["networks"];
        assert_eq!(
            api["regere-dev-platform-network"]["aliases"][0],
            "wl-acme-dev-001-orders-api"
        );
        assert_eq!(api["app"], yaml("{}"));
        assert!(
            c["services"]["postgres"]["networks"]
                .get("regere-dev-platform-network")
                .is_none(),
            "only the attached group joins"
        );
        assert!(
            c["services"][ZT_SIDECAR].get("networks").is_none(),
            "the sidecar stays on the host network"
        );
    }

    #[test]
    fn resource_references_render_with_secrets_apart() {
        let mut input = sample();
        let api = &mut input.services[1];
        let var = |name: &str, from: &str| EnvVar {
            name: name.into(),
            from: Some(from.into()),
            ..Default::default()
        };
        api.spec.env.extend([
            var("SQS_QUEUE_URL", "resource:jobs.url"),
            var("AWS_ENDPOINT_URL", "resource:jobs.endpoint"),
            var("AWS_SECRET_ACCESS_KEY", "resource:jobs.secret_access_key"),
        ]);
        api.resources.insert(
            "jobs".into(),
            ResourceValues {
                api: ResourceApi::Sqs,
                fields: BTreeMap::from([
                    (
                        "url".into(),
                        "https://sqs.ap-south-1.amazonaws.com/1/q".into(),
                    ),
                    ("secret_access_key".into(), "s3cr$t".into()),
                ]),
            },
        );
        let r = render(&input).unwrap();
        let c = yaml(&r.compose);
        let env = &c["services"]["orders-api"]["environment"];
        assert_eq!(
            env["SQS_QUEUE_URL"],
            "https://sqs.ap-south-1.amazonaws.com/1/q"
        );
        assert!(
            env.get("AWS_ENDPOINT_URL").is_none(),
            "no endpoint on AWS itself"
        );
        assert!(env.get("AWS_SECRET_ACCESS_KEY").is_none());
        let secrets = yaml(&r.secrets);
        assert_eq!(
            secrets["services"]["orders-api"]["environment"]["AWS_SECRET_ACCESS_KEY"],
            "s3cr$$t"
        );

        // A field the API doesn't have, and a resource the service doesn't have.
        input.services[1]
            .spec
            .env
            .push(var("BAD", "resource:jobs.bucket"));
        assert!(
            render(&input)
                .unwrap_err()
                .to_string()
                .contains("no field `bucket`")
        );
        input.services[1].spec.env.pop();
        input.services[1]
            .spec
            .env
            .push(var("BAD", "resource:assets.name"));
        assert!(
            render(&input)
                .unwrap_err()
                .to_string()
                .contains("ikit res list")
        );
    }

    #[test]
    fn detects_image_registries() {
        assert_eq!(image_registry("postgres:16"), "docker.io");
        assert_eq!(image_registry("minio/minio:latest"), "docker.io");
        assert_eq!(image_registry("ghcr.io/acme/api:1"), "ghcr.io");
        assert_eq!(image_registry("localhost:5000/api"), "localhost:5000");
        assert_eq!(
            image_registry("123.dkr.ecr.eu-west-1.amazonaws.com/api:1"),
            "123.dkr.ecr.eu-west-1.amazonaws.com"
        );
    }

    fn from_var(name: &str, from: &str) -> EnvVar {
        EnvVar {
            name: name.into(),
            from: Some(from.into()),
            ..Default::default()
        }
    }

    /// A platform workload `wl-acme-dev-002` with `decision-hub` (port 8080) and a postgres.
    fn platform() -> LinkedWorkload {
        let mut hub = instance(
            "decision-hub",
            "decision-hub",
            "1.0.0",
            "ghcr.io/acme/decision-hub:1",
        );
        hub.group = Category::Platform;
        let spec = ServiceSpec {
            ports: vec![Port {
                container: 8080,
                name: Some("http".into()),
                protocol: Protocol::Tcp,
            }],
            ..Default::default()
        };
        let pg = sample().services.remove(0);
        LinkedWorkload {
            workload: "wl-acme-dev-002".into(),
            services: vec![
                ServiceInput {
                    instance: hub,
                    spec,
                    secrets: BTreeMap::new(),
                    urls: vec![],
                    resources: BTreeMap::new(),
                },
                pg,
            ],
        }
    }

    #[test]
    fn templates_fill_references_own_env_and_context() {
        let mut input = sample();
        let api = &mut input.services[1];
        api.spec.env.extend([
            EnvVar {
                name: "DB_NAME".into(),
                default: Some("{{service}}".into()),
                ..Default::default()
            },
            EnvVar {
                name: "DB_USER".into(),
                default: Some("orders".into()),
                ..Default::default()
            },
            from_var(
                "DATABASE_URL",
                "template:postgresql://{{env.DB_USER}}:{{service:postgres.env.POSTGRES_PASSWORD}}@{{service:postgres.host}}:{{service:postgres.port}}/{{env.DB_USER}}_{{env}}",
            ),
            from_var("WHERE", "template:{{org}}/{{workload}}/{{service}}"),
        ]);
        let r = render(&input).unwrap();
        let c = yaml(&r.compose);
        let env = &c["services"]["orders-api"]["environment"];
        assert_eq!(env["WHERE"], "acme/wl-acme-dev-001/orders-api");
        assert!(
            env.get("DATABASE_URL").is_none(),
            "a secret part makes it secret"
        );
        let sec = yaml(&r.secrets);
        assert_eq!(
            sec["services"]["orders-api"]["environment"]["DATABASE_URL"],
            "postgresql://orders:pa$$$$word@postgres:5432/orders_dev"
        );
    }

    #[test]
    fn templates_report_loops_and_unknown_vars() {
        let mut input = sample();
        input.services[1].spec.env.extend([
            from_var("A", "template:{{env.B}}"),
            from_var("B", "template:{{env.A}}"),
        ]);
        let err = render(&input).unwrap_err().to_string();
        assert!(err.contains("loop"), "{err}");
    }

    #[test]
    fn linked_services_resolve_to_their_prefixed_alias() {
        let mut input = sample();
        input.linked = vec![platform()];
        input.services[1].spec.env.extend([
            from_var("HUB", "service:decision-hub.internal_url"),
            from_var("HUB_HOST", "service:wl-acme-dev-002/decision-hub.host"),
        ]);
        let c = yaml(&render(&input).unwrap().compose);
        let api = &c["services"]["orders-api"];
        assert_eq!(
            api["environment"]["HUB"],
            "http://wl-acme-dev-002-decision-hub:8080"
        );
        assert_eq!(
            api["environment"]["HUB_HOST"],
            "wl-acme-dev-002-decision-hub"
        );
        // The local postgres still wins over the linked workload's.
        assert_eq!(api["environment"]["DB_HOST"], "postgres");
        // App services join the provider's link network; data services do not.
        assert_eq!(api["networks"]["wl-acme-dev-002-link"], yaml("{}"));
        assert!(
            c["services"]["postgres"]["networks"]
                .get("wl-acme-dev-002-link")
                .is_none()
        );
        assert_eq!(c["networks"]["wl-acme-dev-002-link"]["external"], true);
    }

    #[test]
    fn linked_data_services_are_not_reachable() {
        let mut input = sample();
        input.services.remove(0);
        input.services[0].spec.env.retain(|e| e.from.is_none());
        input.services[0].spec.depends_on.clear();
        input.linked = vec![platform()];
        input.services[0]
            .spec
            .env
            .push(from_var("PG", "service:postgres.host"));
        let err = render(&input).unwrap_err().to_string();
        assert!(err.contains("data services are not shared"), "{err}");
    }

    #[test]
    fn a_provider_offers_its_link_network_with_aliases() {
        let mut input = sample();
        input.offers_link = true;
        let c = yaml(&render(&input).unwrap().compose);
        assert_eq!(
            c["services"]["orders-api"]["networks"]["wl-acme-dev-001-link"]["aliases"][0],
            "wl-acme-dev-001-orders-api"
        );
        assert!(
            c["services"]["postgres"]["networks"]
                .get("wl-acme-dev-001-link")
                .is_none()
        );
    }

    #[test]
    fn host_aliases_and_shm_size_render() {
        let mut input = sample();
        input.host_aliases = vec!["dev-001.localhost".into()];
        input.services[1].spec.shm_size = Some("2gb".into());
        let c = yaml(&render(&input).unwrap().compose);
        let api = &c["services"]["orders-api"];
        assert_eq!(api["extra_hosts"][0], "dev-001.localhost:host-gateway");
        assert_eq!(api["shm_size"], "2gb");
        assert!(c["services"]["postgres"].get("extra_hosts").is_none());
    }

    #[test]
    fn a_missing_generated_secret_says_how_to_create_it() {
        let mut input = sample();
        input.services[1].spec.env.push(EnvVar {
            name: "KEY".into(),
            secret: true,
            generate: Some(ikit_core::Generate::Hex32),
            ..Default::default()
        });
        let err = render(&input).unwrap_err().to_string();
        assert!(err.contains("--generate KEY"), "{err}");
    }
}
