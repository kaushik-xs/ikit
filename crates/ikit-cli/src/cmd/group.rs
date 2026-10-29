use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Context, bail};
use clap::Subcommand;
use ikit_app::{App, RegistryFilter, ServiceRequest};
use ikit_core::{Category, Dependency, RegistryEntry, RegistryVersion, VersionConstraint};
use inquire::{Confirm, MultiSelect, Password, PasswordDisplayMode, Select, Text};

use super::service::{parse_assignment, print_table, secret_env_name};
use crate::output::{Output, print_json, table};
use crate::{Ctx, warn};

#[derive(Debug, Subcommand)]
pub enum GroupCmd {
    /// Add services from the registry to a group (created if needed). Without --services,
    /// pick them interactively.
    Add {
        kind: Category,
        /// SERVICE[@CONSTRAINT] list, e.g. postgres@16,redis. Constraints: latest, 1.4.2, 16, ^1.4.
        #[arg(long, value_delimiter = ',')]
        services: Vec<Dependency>,
        /// Add missing dependencies without asking.
        #[arg(long)]
        with_deps: bool,
        /// Offer services from every category, not just this group's.
        #[arg(long)]
        all_categories: bool,
        /// Env value: SERVICE.KEY=VALUE (repeatable). Secrets come from prompts or
        /// IKIT_SECRET_<SERVICE>_<KEY>.
        #[arg(long = "set", value_name = "SERVICE.KEY=VALUE")]
        set: Vec<String>,
    },
    /// List groups and their services.
    #[command(alias = "ls")]
    List,
    /// Remove a group with all its services.
    #[command(alias = "remove")]
    Rm {
        kind: Category,
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(ctx: &Ctx, cmd: GroupCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    let wl = app.resolve_workload_name(ctx.wl.as_deref())?;
    match cmd {
        GroupCmd::Add {
            kind,
            services,
            with_deps,
            all_categories,
            set,
        } => {
            add(
                ctx,
                &app,
                &wl,
                kind,
                services,
                with_deps,
                all_categories,
                set,
            )
            .await
        }
        GroupCmd::List => {
            let services = app.services_list(&wl).await?;
            if ctx.output == Output::Json {
                let mut groups: BTreeMap<Category, Vec<&str>> = BTreeMap::new();
                for s in &services {
                    groups.entry(s.group).or_default().push(&s.name);
                }
                return print_json(&groups);
            }
            if services.is_empty() {
                println!(
                    "{wl} has no service groups yet. Add one with `ikit sg add <app|platform|data>`."
                );
                return Ok(());
            }
            let mut t = table(&["GROUP", "SERVICES"]);
            for kind in Category::ALL {
                let names: Vec<String> = services
                    .iter()
                    .filter(|s| s.group == *kind)
                    .map(|s| format!("{} ({}@{})", s.name, s.registry_service, s.version))
                    .collect();
                if !names.is_empty() {
                    t.add_row(vec![kind.to_string(), names.join("\n")]);
                }
            }
            println!("{t}");
            Ok(())
        }
        GroupCmd::Rm { kind, yes } => {
            if ctx.confirm(
                &format!("Remove the {kind} group and all its services from {wl}?"),
                yes,
            )? {
                let removed = app.group_remove(&wl, kind).await?;
                if ctx.output == Output::Table {
                    println!("Removed {kind} group ({}).", removed.join(", "));
                }
            }
            Ok(())
        }
    }
}

/// A service picked for adding, resolved against the registry.
struct Pending {
    name: String,
    group: Category,
    constraint: VersionConstraint,
    entry: RegistryEntry,
    version: RegistryVersion,
}

#[allow(clippy::too_many_arguments)]
async fn add(
    ctx: &Ctx,
    app: &App,
    wl: &str,
    kind: Category,
    services: Vec<Dependency>,
    with_deps: bool,
    all_categories: bool,
    set: Vec<String>,
) -> anyhow::Result<()> {
    let picked = if !services.is_empty() {
        services
    } else if ctx.interactive {
        pick_services(app, kind, all_categories).await?
    } else {
        bail!("--services is required when prompts are disabled");
    };

    let mut pending = Vec::new();
    for dep in picked {
        let (entry, version) = app.registry_resolve(&dep.name, &dep.constraint).await?;
        pending.push(Pending {
            name: dep.name,
            group: kind,
            constraint: dep.constraint,
            entry,
            version,
        });
    }
    add_dependencies(ctx, app, wl, &mut pending, with_deps).await?;

    let mut values = parse_sets(&set, &pending)?;
    let mut requests = Vec::new();
    for p in &pending {
        let env = collect_env(ctx, p, values.remove(&p.name).unwrap_or_default())?;
        requests.push(ServiceRequest {
            name: Some(p.name.clone()),
            registry: p.entry.service.name.clone(),
            constraint: p.constraint.clone(),
            group: p.group,
            env,
        });
    }

    let report = app.services_add(wl, requests).await?;
    if ctx.output == Output::Json {
        return print_json(&report);
    }
    println!("Added to {wl}:");
    print_table(&report.added);
    for u in &report.unmet {
        warn(format!(
            "{} depends on {}, which no service in {wl} provides",
            u.service, u.dependency
        ));
    }
    println!("Spec written to {}.", app.org().workload_spec(wl).display());
    Ok(())
}

async fn pick_services(
    app: &App,
    kind: Category,
    all_categories: bool,
) -> anyhow::Result<Vec<Dependency>> {
    let category = (!all_categories).then_some(kind);
    let entries = app
        .registry_list(&RegistryFilter {
            category,
            tag: None,
        })
        .await?;
    let entries: Vec<RegistryEntry> = entries
        .into_iter()
        .filter(|e| e.latest().is_some())
        .collect();
    if entries.is_empty() {
        bail!(
            "the registry has no {kind} services; add one with `ikit registry add` or pass --all-categories"
        );
    }

    struct ServiceChoice(RegistryEntry);
    impl fmt::Display for ServiceChoice {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let s = &self.0.service;
            let latest = self
                .0
                .latest()
                .map(|v| v.version.to_string())
                .unwrap_or_default();
            write!(
                f,
                "{:<16} {latest:<12} {}",
                s.name,
                s.description.as_deref().unwrap_or("")
            )
        }
    }
    let chosen = MultiSelect::new(
        &format!("Services for the {kind} group:"),
        entries.into_iter().map(ServiceChoice).collect(),
    )
    .with_help_message("space to select, enter to confirm, type to filter")
    .prompt()?;
    if chosen.is_empty() {
        bail!("no services selected");
    }

    let mut out = Vec::new();
    for ServiceChoice(entry) in chosen {
        let constraint = pick_constraint(&entry)?;
        out.push(Dependency {
            name: entry.service.name,
            constraint,
        });
    }
    Ok(out)
}

/// Version choice: a range on the newest major by default, `latest`, patch-only, or a pin.
fn pick_constraint(entry: &RegistryEntry) -> anyhow::Result<VersionConstraint> {
    struct Choice(String, String);
    impl fmt::Display for Choice {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{:<10} {}", self.0, self.1)
        }
    }
    let latest = entry.latest().context("no stable version")?;
    let v = &latest.version;
    let mut choices = vec![
        Choice(
            format!("^{}", v.major),
            format!("newest {}.x (currently {v})", v.major),
        ),
        Choice(
            format!("~{}.{}", v.major, v.minor),
            format!("patch updates only (currently {v})"),
        ),
        Choice(
            "latest".into(),
            format!("always the newest stable (currently {v})"),
        ),
    ];
    for version in &entry.versions {
        let note = if version.deprecated {
            "pin (deprecated)"
        } else {
            "pin"
        };
        choices.push(Choice(
            version.version.to_string(),
            format!("{note}: {}", version.image),
        ));
    }
    let choice = Select::new(&format!("{} version:", entry.service.name), choices).prompt()?;
    Ok(choice.0.parse()?)
}

/// Offers (or with `--with-deps`, adds) registry services that pending services depend on.
async fn add_dependencies(
    ctx: &Ctx,
    app: &App,
    wl: &str,
    pending: &mut Vec<Pending>,
    with_deps: bool,
) -> anyhow::Result<()> {
    let existing = app.services_list(wl).await?;
    let mut declined = BTreeSet::new();
    let mut i = 0;
    while i < pending.len() {
        let deps = pending[i].version.spec.depends_on.clone();
        let owner = pending[i].name.clone();
        for dep in deps {
            let installed = existing
                .iter()
                .map(|s| (s.registry_service.as_str(), &s.version))
                .chain(
                    pending
                        .iter()
                        .map(|p| (p.entry.service.name.as_str(), &p.version.version)),
                );
            if dep.satisfied_by(installed) || declined.contains(&dep.to_string()) {
                continue;
            }
            let add = with_deps
                || (ctx.interactive
                    && Confirm::new(&format!("{owner} depends on {dep}. Add it?"))
                        .with_default(true)
                        .prompt()?);
            if !add {
                declined.insert(dep.to_string());
                continue;
            }
            let (entry, version) = app.registry_resolve(&dep.name, &dep.constraint).await?;
            if pending.iter().any(|p| p.name == dep.name)
                || existing.iter().any(|s| s.name == dep.name)
            {
                bail!(
                    "{owner} needs {dep}, but a service named {} with another version is already planned",
                    dep.name
                );
            }
            let group = entry.service.category;
            pending.push(Pending {
                name: dep.name.clone(),
                group,
                constraint: dep.constraint,
                entry,
                version,
            });
        }
        i += 1;
    }
    Ok(())
}

/// Groups `SERVICE.KEY=VALUE` assignments by service, rejecting secrets and unknown services.
fn parse_sets(
    set: &[String],
    pending: &[Pending],
) -> anyhow::Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for s in set {
        let (target, value) = parse_assignment(s)?;
        let (service, key) = target
            .split_once('.')
            .with_context(|| format!("expected SERVICE.KEY=VALUE, got `{s}`"))?;
        let p = pending
            .iter()
            .find(|p| p.name == service)
            .with_context(|| {
                format!("--set {s}: `{service}` is not one of the services being added")
            })?;
        if p.version.spec.env_var(key).is_some_and(|e| e.secret) {
            bail!(
                "{service}.{key} is a secret; enter it at the prompt or set {}",
                secret_env_name(service, key)
            );
        }
        out.entry(service.to_owned())
            .or_default()
            .insert(key.to_owned(), value);
    }
    Ok(out)
}

/// Fills env values for one service: `--set` values first, then `IKIT_SECRET_*` for secrets,
/// then prompts for anything still required.
fn collect_env(
    ctx: &Ctx,
    p: &Pending,
    mut env: BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, String>> {
    for var in &p.version.spec.env {
        if env.contains_key(&var.name) || var.from.is_some() {
            continue;
        }
        if var.secret
            && let Ok(v) = std::env::var(secret_env_name(&p.name, &var.name))
        {
            env.insert(var.name.clone(), v);
            continue;
        }
        if !ctx.interactive || !var.needs_value() {
            continue;
        }
        let label = format!("{}.{}:", p.name, var.name);
        let help = var
            .description
            .clone()
            .unwrap_or_else(|| "required".to_owned());
        let value = if var.secret {
            Password::new(&label)
                .with_help_message(&format!("{help} (stored in the keychain)"))
                .with_display_mode(PasswordDisplayMode::Masked)
                .prompt()?
        } else if !var.allowed.is_empty() {
            Select::new(&label, var.allowed.clone())
                .with_help_message(&help)
                .prompt()?
        } else {
            Text::new(&label).with_help_message(&help).prompt()?
        };
        env.insert(var.name.clone(), value);
    }
    Ok(env)
}
