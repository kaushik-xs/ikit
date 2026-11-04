//! Workload → `compose.yaml` (no secrets) + `secrets.compose.yaml` (secret env only).

use std::collections::BTreeMap;

use ikit_core::registry::EnvVar;
use ikit_core::{Category, DockerNetwork, ServiceInstance, ServiceSpec};
use serde::Serialize;

/// One service with everything needed to render it.
#[derive(Debug, Clone)]
pub struct ServiceInput {
    pub instance: ServiceInstance,
    /// The spec of the instance's resolved registry version.
    pub spec: ServiceSpec,
    /// Secret env values from the keychain.
    pub secrets: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct WorkloadInput {
    pub org: String,
    pub workload: String,
    pub services: Vec<ServiceInput>,
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
    resources: DeployResources,
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

/// Resolves `service:<name>.host | .port | .port.<port name> | .env.<VAR>`. `<name>` is a
/// service instance, or a registry service if exactly one instance uses it. A `.host` must
/// be on a network that a service in `from` joins.
fn resolve_ref(
    reference: &str,
    from: Category,
    services: &[ServiceInput],
) -> Result<Resolved, String> {
    let rest = reference.strip_prefix("service:").ok_or_else(|| {
        format!("unsupported reference `{reference}` (expected service:<name>.<field>)")
    })?;
    let (name, field) = rest
        .split_once('.')
        .ok_or_else(|| format!("reference `{reference}` has no field"))?;
    let by_instance = services.iter().find(|s| s.instance.name == name);
    let target = match by_instance {
        Some(t) => t,
        None => {
            let matches: Vec<&ServiceInput> = services
                .iter()
                .filter(|s| s.instance.registry_service == name)
                .collect();
            match matches.as_slice() {
                [one] => *one,
                [] => return Err(format!("`{name}` is not a service in this workload")),
                _ => {
                    return Err(format!(
                        "`{name}` matches several services; use an instance name"
                    ));
                }
            }
        }
    };
    let plain = |value: String| {
        Ok(Resolved {
            value,
            secret: false,
        })
    };
    match field.split_once('.') {
        None if field == "host" => {
            let group = target.instance.group;
            if !reachable_groups(from).contains(&group) {
                return Err(format!(
                    "`{}` is in the {group} group, which a {from} service cannot reach",
                    target.instance.name
                ));
            }
            plain(target.instance.name.clone())
        }
        None if field == "port" => match target.spec.ports.first() {
            Some(p) => plain(p.container.to_string()),
            None => Err(format!("`{}` exposes no ports", target.instance.name)),
        },
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
        Some(("env", var)) => {
            let spec_var = target
                .spec
                .env_var(var)
                .ok_or_else(|| format!("`{}` has no env var {var}", target.instance.name))?;
            direct_value(target, spec_var)
                .ok_or_else(|| format!("`{}` env var {var} has no value", target.instance.name))
        }
        _ => Err(format!(
            "unknown field `{field}` in `{reference}` (use host, port, port.<name> or env.<VAR>)"
        )),
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

    for s in &input.services {
        let inst = &s.instance;
        let mut environment = BTreeMap::new();
        let mut secret_env = BTreeMap::new();
        for var in &s.spec.env {
            let resolved = match &var.from {
                // A value set on the instance wins over the reference.
                Some(_)
                    if inst.env.contains_key(&var.name) || s.secrets.contains_key(&var.name) =>
                {
                    direct_value(s, var)
                }
                Some(reference) => {
                    Some(resolve_ref(reference, inst.group, &input.services).map_err(
                        |message| RenderError::Env {
                            service: inst.name.clone(),
                            var: var.name.clone(),
                            message,
                        },
                    )?)
                }
                None => direct_value(s, var),
            };
            match resolved {
                Some(r) if r.secret || var.secret => {
                    secret_env.insert(var.name.clone(), escape(&r.value))
                }
                Some(r) => environment.insert(var.name.clone(), escape(&r.value)),
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

        let ports = inst
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

        let deploy = s.spec.resources.as_ref().and_then(|r| {
            let mut limits = BTreeMap::new();
            if let Some(cpu) = &r.cpu {
                limits.insert("cpus", cpu.clone());
            }
            if let Some(mem) = &r.memory {
                limits.insert("memory", mem.clone());
            }
            (!limits.is_empty()).then_some(Deploy {
                resources: DeployResources { limits },
            })
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
                command: vec![],
                network_mode: None,
                networks: service_networks,
                cap_add: vec![],
                devices: vec![],
                environment,
                ports,
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
                },
                ServiceInput {
                    instance: api,
                    spec: api_spec,
                    secrets: BTreeMap::new(),
                },
            ],
            zerotier: None,
            external_networks: vec![],
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
}
