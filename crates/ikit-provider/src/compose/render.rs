//! Workload → `compose.yaml` (no secrets) + `secrets.compose.yaml` (secret env only).

use std::collections::BTreeMap;

use ikit_core::registry::EnvVar;
use ikit_core::{ServiceInstance, ServiceSpec};
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
}

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
}

#[derive(Serialize)]
struct ComposeFile {
    name: String,
    services: BTreeMap<String, ComposeService>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    volumes: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Serialize)]
struct ComposeService {
    image: String,
    restart: &'static str,
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
/// service instance, or a registry service if exactly one instance uses it.
fn resolve_ref(reference: &str, services: &[ServiceInput]) -> Result<Resolved, String> {
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
        None if field == "host" => plain(target.instance.name.clone()),
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
                    Some(resolve_ref(reference, &input.services).map_err(|message| {
                        RenderError::Env {
                            service: inst.name.clone(),
                            var: var.name.clone(),
                            message,
                        }
                    })?)
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

    let header = format!(
        "# Generated by ikit for {}/{}. Do not edit; changes are overwritten.\n",
        input.org, input.workload
    );
    let compose = ComposeFile {
        name: input.workload.clone(),
        services,
        volumes,
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
