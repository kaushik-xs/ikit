use std::collections::BTreeMap;

use anyhow::{Context, bail};
use clap::Subcommand;
use ikit_core::{PublishedPort, ServiceInstance, VersionConstraint};
use inquire::{Password, PasswordDisplayMode};

use crate::output::{Output, print_json, table};
use crate::{Ctx, warn};

#[derive(Debug, Subcommand)]
pub enum ServiceCmd {
    /// List the workload's services.
    #[command(alias = "ls")]
    List,
    /// Show one service with its env schema.
    Show { name: String },
    /// Switch a service to the newest version matching its constraint, or --to a new one.
    Upgrade {
        name: String,
        /// New constraint, e.g. ^17 or 17.2.0.
        #[arg(long)]
        to: Option<VersionConstraint>,
    },
    /// Set or unset env values.
    Set {
        name: String,
        /// KEY=VALUE for non-secret vars.
        values: Vec<String>,
        /// Secret var to set; prompts, or reads IKIT_SECRET_<SERVICE>_<KEY> (repeatable).
        #[arg(long = "secret", value_name = "KEY")]
        secrets: Vec<String>,
        #[arg(long = "unset", value_name = "KEY")]
        unset: Vec<String>,
    },
    /// Publish container ports on the target's host, or stop publishing them. Takes effect
    /// on the next `ikit apply`.
    Publish {
        name: String,
        /// [BIND:]HOST:CONTAINER[/udp], e.g. 8080:80 or 127.0.0.1:5432:5432 (repeatable).
        ports: Vec<PublishedPort>,
        /// Host port to stop publishing (repeatable).
        #[arg(long = "remove", value_name = "HOST_PORT")]
        remove: Vec<u16>,
    },
    /// Remove services and their keychain secrets.
    #[command(alias = "remove")]
    Rm {
        #[arg(required = true)]
        names: Vec<String>,
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(ctx: &Ctx, cmd: ServiceCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    let wl = app.resolve_workload_name(ctx.wl.as_deref())?;
    match cmd {
        ServiceCmd::List => {
            let services = app.services_list(&wl).await?;
            if ctx.output == Output::Json {
                return print_json(&services);
            }
            if services.is_empty() {
                println!(
                    "{wl} has no services yet. Add some with `ikit sg add <app|platform|data>`."
                );
            } else {
                print_table(&services);
            }
        }
        ServiceCmd::Show { name } => {
            let (service, version) = app.service_show(&wl, &name).await?;
            if ctx.output == Output::Json {
                return print_json(&serde_json::json!({ "service": service, "version": version }));
            }
            print_table(std::slice::from_ref(&service));
            if !version.spec.env.is_empty() {
                let mut t = table(&["ENV", "VALUE", "SOURCE"]);
                for var in &version.spec.env {
                    let (value, source) = if service.secret_env.contains(&var.name) {
                        ("••••••".to_owned(), "keychain")
                    } else if let Some(v) = service.env.get(&var.name) {
                        (v.clone(), "set")
                    } else if let Some(from) = &var.from {
                        (from.clone(), "reference")
                    } else if let Some(d) = &var.default {
                        (d.clone(), "registry default")
                    } else {
                        (
                            "".to_owned(),
                            if var.required { "MISSING" } else { "unset" },
                        )
                    };
                    t.add_row(vec![var.name.clone(), value, source.to_owned()]);
                }
                println!("{t}");
            }
        }
        ServiceCmd::Upgrade { name, to } => {
            let (old, svc) = app.service_upgrade(&wl, &name, to).await?;
            if ctx.output == Output::Json {
                return print_json(&svc);
            }
            if old == svc.version {
                println!(
                    "{name} is already on {} (newest matching {}).",
                    svc.version, svc.constraint
                );
            } else {
                println!("{name}: {old} → {} ({}).", svc.version, svc.image);
            }
            if svc.deprecated {
                warn(format!(
                    "{}@{} is deprecated",
                    svc.registry_service, svc.version
                ));
            }
        }
        ServiceCmd::Set {
            name,
            values,
            secrets,
            unset,
        } => {
            let mut set = BTreeMap::new();
            for v in &values {
                let (k, v) = parse_assignment(v)?;
                set.insert(k, v);
            }
            let (_, version) = app.service_show(&wl, &name).await?;
            for k in set.keys() {
                if version.spec.env_var(k).is_some_and(|e| e.secret) {
                    bail!("{k} is a secret; use --secret {k}");
                }
            }
            for k in &secrets {
                if !version.spec.env_var(k).is_some_and(|e| e.secret) {
                    bail!("{k} is not a secret env var of {name}; pass it as {k}=VALUE");
                }
                let value = match std::env::var(secret_env_name(&name, k)) {
                    Ok(v) => v,
                    Err(_) if ctx.interactive => Password::new(&format!("{name}.{k}:"))
                        .with_display_mode(PasswordDisplayMode::Masked)
                        .prompt()?,
                    Err(_) => bail!("set {} or run interactively", secret_env_name(&name, k)),
                };
                set.insert(k.clone(), value);
            }
            let svc = app.service_set_env(&wl, &name, set, &unset).await?;
            if ctx.output == Output::Json {
                return print_json(&svc);
            }
            print_table(std::slice::from_ref(&svc));
        }
        ServiceCmd::Publish {
            name,
            ports,
            remove,
        } => {
            if ports.is_empty() && remove.is_empty() {
                bail!("pass a port mapping such as 8080:80, or --remove <host port>");
            }
            let (_, version) = app.service_show(&wl, &name).await?;
            for p in &ports {
                if !version.spec.ports.is_empty()
                    && !version
                        .spec
                        .ports
                        .iter()
                        .any(|sp| sp.container == p.container)
                {
                    warn(format!(
                        "{name} does not declare container port {} in the registry",
                        p.container
                    ));
                }
            }
            let svc = app.service_publish(&wl, &name, ports, &remove).await?;
            if ctx.output == Output::Json {
                return print_json(&svc);
            }
            let ports: Vec<String> = svc.publish.iter().map(ToString::to_string).collect();
            let ports = if ports.is_empty() {
                "nothing".to_owned()
            } else {
                ports.join(", ")
            };
            println!("{name} publishes {ports}. Run `ikit apply` to make it take effect.");
        }
        ServiceCmd::Rm { names, yes } => {
            if ctx.confirm(&format!("Remove {} from {wl}?", names.join(", ")), yes)? {
                app.services_remove(&wl, &names).await?;
                if ctx.output == Output::Table {
                    println!("Removed {}.", names.join(", "));
                }
            }
        }
    }
    Ok(())
}

pub fn print_table(services: &[ServiceInstance]) {
    let mut t = table(&[
        "NAME", "GROUP", "SERVICE", "VERSION", "IMAGE", "PUBLISH", "ENV", "SECRETS",
    ]);
    for s in services {
        let env: Vec<String> = s.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut version = format!("{} → {}", s.constraint, s.version);
        if s.deprecated {
            version.push_str(" (deprecated)");
        }
        t.add_row(vec![
            s.name.clone(),
            s.group.to_string(),
            s.registry_service.clone(),
            version,
            s.image.clone(),
            s.publish
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            env.join("\n"),
            s.secret_env.join("\n"),
        ]);
    }
    println!("{t}");
}

/// `IKIT_SECRET_<SERVICE>_<KEY>`, with `-` turned into `_`.
pub fn secret_env_name(service: &str, key: &str) -> String {
    format!("IKIT_SECRET_{}_{}", service.replace('-', "_"), key).to_ascii_uppercase()
}

pub fn parse_assignment(s: &str) -> anyhow::Result<(String, String)> {
    let (k, v) = s
        .split_once('=')
        .with_context(|| format!("expected KEY=VALUE, got `{s}`"))?;
    Ok((k.trim().to_owned(), v.to_owned()))
}
