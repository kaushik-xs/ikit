use std::fs;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use ikit_app::{AddVersion, RegistryFilter, ServiceChanges, VersionChanges};
use ikit_core::registry::{
    EnvVar, ImportMode, ImportReport, Port, Protocol, RegistryFile, ServiceDef, VersionDef, Volume,
    is_moving_tag,
};
use ikit_core::version::parse_version;
use ikit_core::{Category, Dependency, RegistryEntry, ServiceSpec};
use semver::Version;

use crate::output::{Output, print_json, table};
use crate::{Ctx, warn};

// Parsed once per run, so variant size does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum RegistryCmd {
    /// List registry services.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        category: Option<Category>,
        #[arg(long)]
        tag: Option<String>,
    },
    /// Show a service and all its versions.
    Show { name: String },
    /// Add a service with its first version. For full specs, use `ikit registry import`.
    Add(AddArgs),
    /// Change a service's details: description, tags, category, owner, homepage.
    Set(SetArgs),
    /// Manage versions of a service.
    #[command(subcommand)]
    Version(VersionCmd),
    /// Remove a service and all its versions (refused while workloads use it).
    #[command(alias = "remove")]
    Rm {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// Import services from a registry.yaml file.
    Import {
        file: PathBuf,
        /// Also delete services and versions missing from the file (in-use entries are kept).
        #[arg(long)]
        replace: bool,
    },
    /// Write the registry as YAML (to stdout, or --file).
    Export {
        #[arg(long, short)]
        file: Option<PathBuf>,
    },
    /// Show which workload services use a registry service.
    Usage { name: String },
    /// Workload services on older or deprecated versions (run `registry sync` first to
    /// learn about new upstream tags). Exits 2 when any are found.
    Outdated {
        /// Only the selected workload (--wl or the active one) instead of the whole org.
        #[arg(long)]
        current: bool,
    },
    /// Find image tags newer than a service's newest version (and add them with --add).
    Sync {
        name: String,
        /// Add the new versions, copying the newest version's spec.
        #[arg(long)]
        add: bool,
        /// Include prerelease tags (e.g. 2.0.0-rc.1).
        #[arg(long)]
        pre: bool,
    },
    /// Store credentials for a private image registry; `ikit apply` logs the target in
    /// before pulling. The password or token is read from a prompt or IKIT_REGISTRY_PASSWORD.
    Login {
        /// Registry host, e.g. ghcr.io or 123456789012.dkr.ecr.eu-west-1.amazonaws.com.
        host: String,
        #[arg(long, short)]
        username: String,
    },
    /// List stored registry logins.
    Logins,
    /// Remove a registry login and its keychain entry.
    Logout { host: String },
}

#[derive(Debug, Args)]
pub struct AddArgs {
    name: String,
    #[arg(long)]
    category: Category,
    /// First version, e.g. 1.4.2 (16 and 16.4 are expanded to 16.0.0 and 16.4.0).
    #[arg(long, value_parser = parse_version_arg)]
    version: Version,
    /// Image reference, e.g. ghcr.io/acme/orders-api:1.4.2.
    #[arg(long)]
    image: String,
    #[command(flatten)]
    spec: SpecArgs,
    #[arg(long)]
    display_name: Option<String>,
    #[arg(long)]
    description: Option<String>,
    /// Tag for filtering (repeatable).
    #[arg(long = "tag")]
    tags: Vec<String>,
    #[arg(long)]
    owner: Option<String>,
    #[arg(long)]
    homepage: Option<String>,
}

/// Spec fields that can be given as flags. Anything richer goes through `registry import`.
#[derive(Debug, Args)]
pub struct SpecArgs {
    /// Container port: PORT[:NAME][/udp] (repeatable).
    #[arg(long = "port", value_name = "PORT[:NAME][/udp]", value_parser = parse_port)]
    ports: Vec<Port>,
    /// Env var: NAME (required) or NAME=DEFAULT (optional) (repeatable).
    #[arg(long = "env", value_name = "NAME[=DEFAULT]")]
    env: Vec<String>,
    /// Required secret env var, stored in the keychain per workload (repeatable).
    #[arg(long = "secret-env", value_name = "NAME")]
    secret_env: Vec<String>,
    /// Volume: NAME:/container/path (repeatable).
    #[arg(long = "volume", value_name = "NAME:/PATH", value_parser = parse_volume)]
    volumes: Vec<Volume>,
    /// Dependency: SERVICE[@CONSTRAINT], e.g. postgres@^16 (repeatable).
    #[arg(long = "depends-on", value_name = "SERVICE[@CONSTRAINT]")]
    depends_on: Vec<Dependency>,
}

#[derive(Debug, Args)]
pub struct SetArgs {
    name: String,
    /// Pass "" to clear.
    #[arg(long)]
    display_name: Option<String>,
    /// Pass "" to clear.
    #[arg(long)]
    description: Option<String>,
    /// Pass "" to clear.
    #[arg(long)]
    owner: Option<String>,
    /// Pass "" to clear.
    #[arg(long)]
    homepage: Option<String>,
    #[arg(long)]
    category: Option<Category>,
    /// Replace all tags (repeatable).
    #[arg(long = "tag")]
    tags: Option<Vec<String>>,
    #[arg(long = "add-tag")]
    add_tags: Vec<String>,
    #[arg(long = "remove-tag")]
    remove_tags: Vec<String>,
}

#[derive(Debug, Args)]
pub struct VersionSetArgs {
    name: String,
    #[arg(value_parser = parse_version_arg)]
    version: Version,
    #[arg(long)]
    image: Option<String>,
    /// Image digest, e.g. sha256:...; pass "" to clear.
    #[arg(long)]
    digest: Option<String>,
    /// Release notes; pass "" to clear.
    #[arg(long)]
    notes: Option<String>,
    /// Mark deprecated: hidden from `latest` and ranges, still usable when pinned.
    #[arg(long, conflicts_with = "undeprecate")]
    deprecate: bool,
    /// Clear the deprecation.
    #[arg(long)]
    undeprecate: bool,
    #[command(flatten)]
    spec: SpecArgs,
    #[arg(long = "remove-env", value_name = "NAME")]
    remove_env: Vec<String>,
    #[arg(long = "remove-port", value_name = "PORT")]
    remove_ports: Vec<u16>,
    #[arg(long = "remove-volume", value_name = "NAME")]
    remove_volumes: Vec<String>,
    #[arg(long = "remove-depends-on", value_name = "SERVICE")]
    remove_depends_on: Vec<String>,
    /// Skip the confirmation when workloads use this version.
    #[arg(long, short)]
    yes: bool,
}

impl SpecArgs {
    fn is_empty(&self) -> bool {
        self.ports.is_empty()
            && self.env.is_empty()
            && self.secret_env.is_empty()
            && self.volumes.is_empty()
            && self.depends_on.is_empty()
    }

    fn into_spec(self) -> ServiceSpec {
        let mut env: Vec<EnvVar> = self
            .env
            .into_iter()
            .map(|e| match e.split_once('=') {
                Some((name, default)) => EnvVar {
                    name: name.to_owned(),
                    default: Some(default.to_owned()),
                    ..Default::default()
                },
                None => EnvVar {
                    name: e,
                    required: true,
                    ..Default::default()
                },
            })
            .collect();
        env.extend(self.secret_env.into_iter().map(|name| EnvVar {
            name,
            required: true,
            secret: true,
            ..Default::default()
        }));
        ServiceSpec {
            ports: self.ports,
            env,
            volumes: self.volumes,
            depends_on: self.depends_on,
            ..Default::default()
        }
    }
}

// Parsed once per run, so variant size does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum VersionCmd {
    /// Change an existing version in place: image, digest, notes, or spec entries.
    /// Spec flags add or replace entries by key (env name, port, volume, dependency).
    Set(VersionSetArgs),
    /// Add a version. Without spec flags, the spec is copied from --from or the newest version.
    Add {
        name: String,
        #[arg(value_parser = parse_version_arg)]
        version: Version,
        #[arg(long)]
        image: String,
        #[arg(long)]
        digest: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        /// Version to copy the spec from.
        #[arg(long, value_parser = parse_version_arg)]
        from: Option<Version>,
        #[command(flatten)]
        spec: SpecArgs,
    },
    /// Pin a version to its image's current digest (docker buildx imagetools), so deploys
    /// use `image@sha256:…`. Required for envs in the org's `require_digest_envs`.
    Pin {
        name: String,
        #[arg(value_parser = parse_version_arg)]
        version: Version,
    },
    /// Mark a version deprecated: hidden from `latest` and ranges, still usable when pinned.
    Deprecate {
        name: String,
        #[arg(value_parser = parse_version_arg)]
        version: Version,
        /// Clear the deprecation instead.
        #[arg(long)]
        undo: bool,
    },
    /// Remove a version (refused while workloads use it).
    #[command(alias = "remove")]
    Rm {
        name: String,
        #[arg(value_parser = parse_version_arg)]
        version: Version,
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(ctx: &Ctx, cmd: RegistryCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    match cmd {
        RegistryCmd::List { category, tag } => {
            let entries = app.registry_list(&RegistryFilter { category, tag }).await?;
            if ctx.output == Output::Json {
                return print_json(&entries);
            }
            if entries.is_empty() {
                println!("No registry services match.");
                return Ok(());
            }
            let mut t = table(&[
                "NAME",
                "CATEGORY",
                "LATEST",
                "VERSIONS",
                "TAGS",
                "DESCRIPTION",
            ]);
            for e in &entries {
                t.add_row(vec![
                    e.service.name.clone(),
                    e.service.category.to_string(),
                    e.latest().map_or("-".to_owned(), |v| v.version.to_string()),
                    e.versions.len().to_string(),
                    e.service.tags.join(", "),
                    e.service.description.clone().unwrap_or_default(),
                ]);
            }
            println!("{t}");
        }
        RegistryCmd::Show { name } => render_entry(ctx, &app.registry_show(&name).await?)?,
        RegistryCmd::Add(args) => {
            warn_moving_tag(&args.image);
            let def = ServiceDef {
                name: args.name,
                display_name: args.display_name,
                description: args.description,
                category: args.category,
                tags: args.tags,
                owner: args.owner,
                homepage: args.homepage,
                versions: vec![VersionDef::new(
                    args.version,
                    args.image,
                    args.spec.into_spec(),
                )],
            };
            render_entry(ctx, &app.registry_add(def).await?)?;
        }
        RegistryCmd::Version(VersionCmd::Add {
            name,
            version,
            image,
            digest,
            notes,
            from,
            spec,
        }) => {
            warn_moving_tag(&image);
            if from.is_some() && !spec.is_empty() {
                bail!("pass either --from or spec flags, not both");
            }
            let spec = (!spec.is_empty()).then(|| spec.into_spec());
            let req = AddVersion {
                version,
                image,
                digest,
                release_notes: notes,
                spec,
                copy_from: from,
            };
            let added = app.registry_version_add(&name, req).await?;
            if ctx.output == Output::Json {
                return print_json(&added);
            }
            println!("Added {name}@{} ({}).", added.version, added.image);
        }
        RegistryCmd::Version(VersionCmd::Deprecate {
            name,
            version,
            undo,
        }) => {
            app.registry_deprecate(&name, &version, !undo).await?;
            if ctx.output == Output::Table {
                let state = if undo {
                    "no longer deprecated"
                } else {
                    "deprecated"
                };
                println!("{name}@{version} is {state}.");
            }
        }
        RegistryCmd::Version(VersionCmd::Rm { name, version, yes }) => {
            if ctx.confirm(&format!("Remove {name}@{version} from the registry?"), yes)? {
                app.registry_version_remove(&name, &version).await?;
                if ctx.output == Output::Table {
                    println!("Removed {name}@{version}.");
                }
            }
        }
        RegistryCmd::Set(args) => {
            let changes = ServiceChanges {
                display_name: clearable(args.display_name),
                description: clearable(args.description),
                owner: clearable(args.owner),
                homepage: clearable(args.homepage),
                category: args.category,
                tags: args.tags,
                add_tags: args.add_tags,
                remove_tags: args.remove_tags,
            };
            render_entry(ctx, &app.registry_update(&args.name, changes).await?)?;
        }
        RegistryCmd::Version(VersionCmd::Set(args)) => {
            let VersionSetArgs {
                name,
                version,
                image,
                digest,
                notes,
                deprecate,
                undeprecate,
                spec,
                remove_env,
                remove_ports,
                remove_volumes,
                remove_depends_on,
                yes,
            } = args;
            if let Some(image) = &image {
                warn_moving_tag(image);
            }
            let users: Vec<String> = app
                .registry_usage(&name)
                .await?
                .into_iter()
                .filter(|u| u.version == version)
                .map(|u| format!("{}/{}", u.workload, u.service))
                .collect();
            if !users.is_empty()
                && !ctx.confirm(
                    &format!(
                        "{name}@{version} is used by {}. Change it in place?",
                        users.join(", ")
                    ),
                    yes,
                )?
            {
                return Ok(());
            }
            let spec = spec.into_spec();
            let changes = VersionChanges {
                image,
                digest: clearable(digest),
                release_notes: clearable(notes),
                deprecated: (deprecate || undeprecate).then_some(deprecate),
                ports: spec.ports,
                env: spec.env,
                volumes: spec.volumes,
                depends_on: spec.depends_on,
                remove_ports,
                remove_env,
                remove_volumes,
                remove_depends_on,
            };
            let update = app
                .registry_version_update(&name, &version, changes)
                .await?;
            if ctx.output == Output::Json {
                return print_json(&update);
            }
            println!("Updated {name}@{version}.");
            if !update.affected.is_empty() {
                println!("Specs rewritten for: {}.", update.affected.join(", "));
            }
            render_entry(ctx, &app.registry_show(&name).await?)?;
        }
        RegistryCmd::Rm { name, yes } => {
            if ctx.confirm(
                &format!("Remove registry service {name} and all its versions?"),
                yes,
            )? {
                app.registry_remove(&name).await?;
                if ctx.output == Output::Table {
                    println!("Removed {name}.");
                }
            }
        }
        RegistryCmd::Import { file, replace } => {
            let yaml =
                fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let registry: RegistryFile = serde_yaml_ng::from_str(&yaml)
                .with_context(|| format!("parsing {}", file.display()))?;
            for s in &registry.services {
                for v in &s.versions {
                    if is_moving_tag(&v.image) {
                        warn(format!(
                            "{}@{} uses a moving image tag ({})",
                            s.name, v.version, v.image
                        ));
                    }
                }
            }
            let mode = if replace {
                ImportMode::Replace
            } else {
                ImportMode::Merge
            };
            let report = app.registry_import(&registry, mode).await?;
            render_import(ctx, &report)?;
        }
        RegistryCmd::Export { file } => {
            let yaml = serde_yaml_ng::to_string(&app.registry_export().await?)?;
            match file {
                Some(path) => {
                    fs::write(&path, yaml)
                        .with_context(|| format!("writing {}", path.display()))?;
                    eprintln!("Wrote {}.", path.display());
                }
                None => print!("{yaml}"),
            }
        }
        RegistryCmd::Version(VersionCmd::Pin { name, version }) => {
            let digest = app.registry_pin(&name, &version).await?;
            if ctx.output == Output::Json {
                return print_json(
                    &serde_json::json!({ "service": name, "version": version, "digest": digest }),
                );
            }
            println!("Pinned {name}@{version} to {digest}.");
        }
        RegistryCmd::Sync { name, add, pre } => {
            let report = app.registry_sync(&name, add, pre).await?;
            if ctx.output == Output::Json {
                return print_json(&report);
            }
            println!("{}: {} tags.", report.repository, report.tags_seen);
            if report.new_versions.is_empty() {
                println!("No versions newer than the registry's.");
            }
            for (v, image) in &report.new_versions {
                let mark = if report.added { "added" } else { "new" };
                println!("  {mark:<6} {v:<12} {image}");
            }
            if !report.added && !report.new_versions.is_empty() {
                println!("Add them with `ikit registry sync {name} --add`.");
            }
        }
        RegistryCmd::Login { host, username } => {
            let password = match std::env::var("IKIT_REGISTRY_PASSWORD") {
                Ok(p) => p,
                Err(_) if ctx.interactive => {
                    inquire::Password::new(&format!("Password or token for {host}:"))
                        .with_display_mode(inquire::PasswordDisplayMode::Masked)
                        .without_confirmation()
                        .prompt()?
                }
                Err(_) => bail!("set IKIT_REGISTRY_PASSWORD or run interactively"),
            };
            let login = app.registry_login(&host, &username, &password).await?;
            if ctx.output == Output::Json {
                return print_json(&login);
            }
            println!(
                "Stored login for {host} as {username}; `ikit apply` will use it for {host} images."
            );
        }
        RegistryCmd::Logins => {
            let logins = app.registry_logins().await?;
            if ctx.output == Output::Json {
                return print_json(&logins);
            }
            if logins.is_empty() {
                println!(
                    "No registry logins. Add one with `ikit registry login <host> --username <user>`."
                );
                return Ok(());
            }
            let mut t = table(&["HOST", "USERNAME", "ADDED"]);
            for l in &logins {
                t.add_row(vec![
                    l.host.clone(),
                    l.username.clone(),
                    l.created_at.format("%Y-%m-%d %H:%M").to_string(),
                ]);
            }
            println!("{t}");
        }
        RegistryCmd::Logout { host } => {
            app.registry_logout(&host).await?;
            if ctx.output == Output::Table {
                println!("Removed the login for {host}.");
            }
        }
        RegistryCmd::Outdated { current } => {
            let wl = if current {
                Some(app.resolve_workload_name(ctx.wl.as_deref())?)
            } else {
                ctx.wl.clone()
            };
            let outdated = app.registry_outdated(wl.as_deref()).await?;
            if ctx.output == Output::Json {
                print_json(&outdated)?;
            } else if outdated.is_empty() {
                println!("Every workload service is on the newest registry version.");
            } else {
                let mut t = table(&[
                    "WORKLOAD",
                    "SERVICE",
                    "CONSTRAINT",
                    "CURRENT",
                    "WANTED",
                    "LATEST",
                ]);
                let show = |v: &Option<semver::Version>| {
                    v.as_ref()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "-".into())
                };
                for o in &outdated {
                    let mut current = o.current.to_string();
                    if o.deprecated {
                        current.push_str(" (deprecated)");
                    }
                    t.add_row(vec![
                        o.workload.clone(),
                        o.service.clone(),
                        o.constraint.to_string(),
                        current,
                        show(&o.wanted),
                        show(&o.latest),
                    ]);
                }
                println!("{t}");
                println!(
                    "`ikit svc upgrade <service>` moves to WANTED; `--to <version>` to LATEST."
                );
            }
            if !outdated.is_empty() {
                std::process::exit(2);
            }
        }
        RegistryCmd::Usage { name } => {
            app.registry_show(&name).await?;
            let usage = app.registry_usage(&name).await?;
            if ctx.output == Output::Json {
                return print_json(&usage);
            }
            if usage.is_empty() {
                println!("No workload uses {name}.");
                return Ok(());
            }
            let mut t = table(&["WORKLOAD", "SERVICE", "VERSION"]);
            for u in &usage {
                t.add_row(vec![
                    u.workload.clone(),
                    u.service.clone(),
                    u.version.to_string(),
                ]);
            }
            println!("{t}");
        }
    }
    Ok(())
}

/// An empty string clears an optional field.
fn clearable(value: Option<String>) -> Option<Option<String>> {
    value.map(|v| (!v.is_empty()).then_some(v))
}

fn warn_moving_tag(image: &str) {
    if is_moving_tag(image) {
        warn(format!(
            "{image} uses a moving tag; pin a version tag or digest (prod workloads will require one)"
        ));
    }
}

fn render_entry(ctx: &Ctx, e: &RegistryEntry) -> anyhow::Result<()> {
    if ctx.output == Output::Json {
        return print_json(e);
    }
    let s = &e.service;
    let mut t = table(&[]);
    t.add_row(vec!["name".to_owned(), s.name.clone()]);
    for (k, v) in [
        ("display name", &s.display_name),
        ("description", &s.description),
        ("owner", &s.owner),
        ("homepage", &s.homepage),
    ] {
        if let Some(v) = v {
            t.add_row(vec![k.to_owned(), v.clone()]);
        }
    }
    t.add_row(vec!["category".to_owned(), s.category.to_string()]);
    if !s.tags.is_empty() {
        t.add_row(vec!["tags".to_owned(), s.tags.join(", ")]);
    }
    t.add_row(vec!["source".to_owned(), s.source.to_string()]);
    println!("{t}");

    let mut t = table(&["VERSION", "IMAGE", "PORTS", "ENV", "DEPENDS ON", "STATUS"]);
    for v in &e.versions {
        let ports: Vec<String> = v
            .spec
            .ports
            .iter()
            .map(|p| match &p.name {
                Some(n) => format!("{}:{n}", p.container),
                None => p.container.to_string(),
            })
            .collect();
        let env: Vec<String> = v
            .spec
            .env
            .iter()
            .map(|e| {
                let mut s = e.name.clone();
                if e.secret {
                    s.push_str(" (secret)");
                } else if let Some(d) = &e.default {
                    s.push_str(&format!("={d}"));
                } else if e.required {
                    s.push_str(" (required)");
                }
                s
            })
            .collect();
        let deps: Vec<String> = v.spec.depends_on.iter().map(ToString::to_string).collect();
        t.add_row(vec![
            v.version.to_string(),
            v.image.clone(),
            ports.join("\n"),
            env.join("\n"),
            deps.join("\n"),
            if v.deprecated { "deprecated" } else { "" }.to_owned(),
        ]);
    }
    println!("{t}");
    Ok(())
}

fn render_import(ctx: &Ctx, r: &ImportReport) -> anyhow::Result<()> {
    if ctx.output == Output::Json {
        return print_json(r);
    }
    let rows = [
        ("services added", &r.services_added),
        ("services updated", &r.services_updated),
        ("services removed", &r.services_removed),
        ("versions added", &r.versions_added),
        ("versions updated", &r.versions_updated),
        ("versions removed", &r.versions_removed),
        ("kept (in use)", &r.kept_in_use),
    ];
    if rows.iter().all(|(_, v)| v.is_empty()) {
        println!("Registry already up to date.");
        return Ok(());
    }
    let mut t = table(&["CHANGE", "COUNT", "ENTRIES"]);
    for (label, items) in rows.iter().filter(|(_, v)| !v.is_empty()) {
        t.add_row(vec![
            label.to_string(),
            items.len().to_string(),
            items.join(", "),
        ]);
    }
    println!("{t}");
    Ok(())
}

fn parse_version_arg(s: &str) -> Result<Version, String> {
    parse_version(s).map_err(|e| e.to_string())
}

fn parse_port(s: &str) -> Result<Port, String> {
    let (rest, protocol) = match s.rsplit_once('/') {
        Some((r, "udp")) => (r, Protocol::Udp),
        Some((r, "tcp")) => (r, Protocol::Tcp),
        Some((_, p)) => return Err(format!("unknown protocol `{p}` (tcp or udp)")),
        None => (s, Protocol::Tcp),
    };
    let (port, name) = match rest.split_once(':') {
        Some((p, n)) => (p, Some(n.to_owned())),
        None => (rest, None),
    };
    let container = port.parse().map_err(|_| format!("invalid port `{port}`"))?;
    Ok(Port {
        container,
        name,
        protocol,
    })
}

fn parse_volume(s: &str) -> Result<Volume, String> {
    let (name, path) = s.split_once(':').ok_or("expected NAME:/PATH")?;
    Ok(Volume {
        name: name.to_owned(),
        path: path.to_owned(),
    })
}
