use std::path::PathBuf;

use clap::Subcommand;
use ikit_app::{KeyringStore, NewOrg, OrgSummary, RemoteOverrides, RemoteStatus};
use ikit_core::RemoteSpec;

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum OrgCmd {
    /// Create an org with its own state database, seeded registry and config.
    Create {
        name: String,
        #[arg(long)]
        display_name: Option<String>,
        /// Workload name prefix (default: wl-<name>, giving wl-<name>-<env>-<nnn>).
        #[arg(long)]
        prefix: Option<String>,
        /// Environments, comma-separated (default: dev,qa,stg,prod).
        #[arg(long, value_delimiter = ',')]
        envs: Option<Vec<String>>,
    },
    /// List orgs; * marks the active one.
    #[command(alias = "ls")]
    List,
    /// Show an org (default: the active one).
    Show { name: Option<String> },
    /// Make an org active for later commands.
    Use { name: String },
    /// Delete an org's folder (database, registry, targets, profiles) and its keychain
    /// entries. Refused while it has workloads.
    #[command(alias = "remove")]
    Rm {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// Keep the org's state in object storage, so a team shares one copy.
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// Keep the org's secrets in an encrypted file that travels with its state, readable by
    /// the keys you grant.
    #[command(subcommand)]
    Secrets(SecretsCmd),
    /// Create a local org from remote state, e.g. s3://bucket/ikit/state/<org>.
    Clone {
        /// s3://bucket/prefix, az://container/prefix, gs://bucket/prefix or local:/folder/prefix.
        url: String,
        /// S3: named profile in ~/.aws/credentials (default: the environment).
        #[arg(long)]
        aws_profile: Option<String>,
        /// S3: region (default: the AWS profile's, else us-east-1).
        #[arg(long)]
        region: Option<String>,
        /// S3-compatible endpoint, e.g. http://localhost:9000 for MinIO.
        #[arg(long)]
        endpoint: Option<String>,
        /// Azure: storage account.
        #[arg(long)]
        account: Option<String>,
        /// GCS: service account key file.
        #[arg(long)]
        credentials_file: Option<PathBuf>,
    },
    /// Bring the local copy up to the remote's latest generation. Other commands do this
    /// themselves; use it to refresh or to drop local changes.
    Pull {
        /// Replace local changes that were never pushed.
        #[arg(long)]
        discard: bool,
    },
    /// Compare the local copy with the remote and show who holds the lock.
    Status,
    /// Remove the remote lock left by a crashed or offline writer.
    Unlock {
        /// Required: removes the lock whoever holds it.
        #[arg(long)]
        force: bool,
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretsCmd {
    /// Move the org's secrets from the keychain into secrets/org.age, readable by this
    /// machine's key and any --recipient. Keychain copies stay until `forget-keychain`.
    Init {
        /// Another public key that can read the secrets, e.g. a recovery key (repeatable).
        #[arg(long = "recipient", value_name = "KEY")]
        recipients: Vec<String>,
    },
    /// Show who can read the org's secrets.
    #[command(alias = "ls")]
    List,
    /// Let a public key (age1… or ssh-ed25519 …) read the org's secrets.
    Grant {
        key: String,
        /// Whose key it is, e.g. alice@rg2k.
        #[arg(long = "as", value_name = "LABEL")]
        label: Option<String>,
    },
    /// Stop a key (or label) from reading the secrets. Rotate what it could read: it may
    /// have kept a copy.
    Revoke {
        key_or_label: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// Delete the keychain copies of secrets the encrypted file now holds.
    ForgetKeychain {
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum RemoteCmd {
    /// Push the org to <storage prefix>/state/<org> in a storage, as generation 1. The
    /// storage's profile must not keep keys in the keychain (use `aws_profile`).
    Set {
        /// Storage to keep the state in (see `ikit storage list`).
        #[arg(long)]
        storage: String,
        /// S3: named profile in ~/.aws/credentials, instead of the storage profile's.
        #[arg(long)]
        aws_profile: Option<String>,
        #[arg(long)]
        region: Option<String>,
    },
    /// Show where the org's remote state lives.
    Show,
    /// Make the org local-only again; the remote state is left as it is.
    #[command(alias = "remove")]
    Rm {
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(ctx: &Ctx, cmd: OrgCmd) -> anyhow::Result<()> {
    match cmd {
        OrgCmd::Create {
            name,
            display_name,
            prefix,
            envs,
        } => {
            let org = ikit_app::org_create(
                &ctx.paths,
                NewOrg {
                    name,
                    display_name,
                    prefix,
                    envs,
                },
            )
            .await?;
            if ctx.output == Output::Json {
                return print_json(&org);
            }
            render(&org);
            let next = if org.active {
                format!("`{}` is now the active org.", org.name)
            } else {
                format!("Switch to it with `ikit org use {}`.", org.name)
            };
            println!("{next} Workloads will be named {}-<env>-<nnn>.", org.prefix);
        }
        OrgCmd::List => {
            let orgs = ikit_app::org_list(&ctx.paths).await?;
            if ctx.output == Output::Json {
                return print_json(&orgs);
            }
            if orgs.is_empty() {
                println!("No orgs yet. Create one with `ikit org create <name>`.");
                return Ok(());
            }
            let mut t = table(&["", "ORG", "NAME", "PREFIX", "ENVS", "WORKLOADS"]);
            for o in &orgs {
                t.add_row(vec![
                    if o.active { "*" } else { "" }.to_owned(),
                    o.name.clone(),
                    o.display_name.clone().unwrap_or_default(),
                    o.prefix.clone(),
                    o.envs.join(", "),
                    o.workloads.to_string(),
                ]);
            }
            println!("{t}");
        }
        OrgCmd::Show { name } => {
            let name = match name {
                Some(n) => n,
                None => ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?,
            };
            let org = ikit_app::org_show(&ctx.paths, &name).await?;
            if ctx.output == Output::Json {
                return print_json(&org);
            }
            render(&org);
        }
        OrgCmd::Use { name } => {
            ikit_app::org_use(&ctx.paths, &name)?;
            if ctx.output == Output::Table {
                println!("Now using org `{name}`.");
            }
        }
        OrgCmd::Rm { name, yes } => {
            let question = format!(
                "Delete org `{name}`: its state database, registry, targets, profiles and keychain entries?"
            );
            if ctx.confirm(&question, yes)? {
                ikit_app::org_remove(&ctx.paths, &name, &KeyringStore).await?;
                if ctx.output == Output::Table {
                    println!("Deleted org `{name}`.");
                }
            }
        }
        OrgCmd::Remote(cmd) => remote(ctx, cmd).await?,
        OrgCmd::Secrets(cmd) => secrets(ctx, cmd).await?,
        OrgCmd::Clone {
            url,
            aws_profile,
            region,
            endpoint,
            account,
            credentials_file,
        } => {
            let mut spec = RemoteSpec::parse_url(&url).map_err(anyhow::Error::msg)?;
            spec.aws_profile = aws_profile;
            spec.region = region;
            spec.endpoint = endpoint;
            spec.account = account;
            spec.credentials_file = credentials_file;
            let (name, head) = ikit_app::org_clone(&ctx.paths, spec).await?;
            if ctx.output == Output::Json {
                return print_json(&head);
            }
            println!(
                "Cloned org `{name}` at generation {} (last written by {} at {}).",
                head.generation,
                head.written_by,
                head.written_at.format("%Y-%m-%d %H:%M UTC")
            );
            let active = ikit_app::resolve_org(&ctx.paths, None).ok();
            if active.as_deref() != Some(name.as_str()) {
                println!("Switch to it with `ikit org use {name}`.");
            }
        }
        OrgCmd::Pull { discard } => {
            let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
            let report = ikit_app::org_pull(&ctx.paths, &org, discard).await?;
            if ctx.output == Output::Json {
                return print_json(&report);
            }
            match report.from {
                Some(from) if from == report.to && !report.discarded_local_changes => {
                    println!("Org `{org}` is up to date at generation {}.", report.to)
                }
                _ => println!(
                    "Pulled org `{org}` to generation {}{}.",
                    report.to,
                    if report.discarded_local_changes {
                        ", discarding local changes"
                    } else {
                        ""
                    }
                ),
            }
        }
        OrgCmd::Status => {
            let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
            let status = ikit_app::org_status(&ctx.paths, &org).await?;
            if ctx.output == Output::Json {
                return print_json(&status);
            }
            render_status(&status, ikit_app::org_uses_secrets_file(&ctx.paths, &org));
        }
        OrgCmd::Unlock { force, yes } => {
            if !force {
                anyhow::bail!(
                    "`ikit org unlock` removes the lock whoever holds it; pass --force to confirm"
                );
            }
            let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
            let Some(held) = ikit_app::org_lock_info(&ctx.paths, &org).await? else {
                println!("Org `{org}` is not locked.");
                return Ok(());
            };
            let question = format!(
                "Remove the lock {} took at {} for `{}`? Only do this if that command is no longer running.",
                held.holder, held.acquired_at, held.command
            );
            if ctx.confirm(&question, yes)? {
                ikit_app::org_unlock(&ctx.paths, &org).await?;
                if ctx.output == Output::Table {
                    println!("Removed the lock on org `{org}`.");
                }
            }
        }
    }
    Ok(())
}

async fn remote(ctx: &Ctx, cmd: RemoteCmd) -> anyhow::Result<()> {
    let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
    match cmd {
        RemoteCmd::Set {
            storage,
            aws_profile,
            region,
        } => {
            let app = ctx.app().await?;
            let overrides = RemoteOverrides {
                aws_profile,
                region,
            };
            let (spec, head, moved) = app
                .remote_set(&storage, overrides, &ctx.identity, &crate::command_line())
                .await?;
            if ctx.output == Output::Json {
                return print_json(&head);
            }
            if let Some(report) = moved {
                println!(
                    "Moved {} secret(s) from the keychain into {}, readable by your key.",
                    report.moved.len(),
                    report.file.display()
                );
            }
            println!(
                "Org `{org}` now keeps its state in {} (generation {}).",
                spec.describe(),
                head.generation
            );
            let flags = match (&spec.region, spec.provider) {
                (Some(r), ikit_core::StorageProvider::S3) => {
                    format!(" --region {r} --aws-profile <theirs>")
                }
                _ => String::new(),
            };
            println!(
                "Teammates join with `ikit org clone {}{flags}`.",
                spec.describe()
            );
        }
        RemoteCmd::Show => {
            let spec = ikit_app::remote_spec(&ctx.paths, &org)?;
            if ctx.output == Output::Json {
                return print_json(&spec);
            }
            let Some(spec) = spec else {
                println!(
                    "Org `{org}` is local-only. Share it with `ikit org remote set --storage <name>`."
                );
                return Ok(());
            };
            let mut t = table(&[]);
            t.add_row(vec!["remote".to_owned(), spec.describe()]);
            for (k, v) in [
                ("region", spec.region.clone()),
                ("endpoint", spec.endpoint.clone()),
                ("account", spec.account.clone()),
                ("aws profile", spec.aws_profile.clone()),
                (
                    "credentials file",
                    spec.credentials_file.map(|p| p.display().to_string()),
                ),
            ] {
                if let Some(v) = v {
                    t.add_row(vec![k.to_owned(), v]);
                }
            }
            println!("{t}");
        }
        RemoteCmd::Rm { yes } => {
            let question = format!(
                "Make org `{org}` local-only? Your copy stops syncing; the remote state is kept."
            );
            if ctx.confirm(&question, yes)? {
                let spec = ikit_app::remote_rm(&ctx.paths, &org)?;
                if ctx.output == Output::Table {
                    println!(
                        "Org `{org}` is local-only now; {} was left as it is.",
                        spec.describe()
                    );
                }
            }
        }
    }
    Ok(())
}

async fn secrets(ctx: &Ctx, cmd: SecretsCmd) -> anyhow::Result<()> {
    let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
    match cmd {
        SecretsCmd::Init { recipients } => {
            let extra = recipients
                .iter()
                .map(|k| ikit_app::SecretsRecipient::parse(k))
                .collect::<Result<Vec<_>, _>>()?;
            let report = ctx.app().await?.secrets_init(&ctx.identity, extra).await?;
            if ctx.output == Output::Json {
                return print_json(&report);
            }
            println!(
                "Moved {} secret(s) for org `{org}` into {}, readable by {} key(s).",
                report.moved.len(),
                report.file.display(),
                report.recipients.len()
            );
            if !report.missing.is_empty() {
                crate::warn(format!(
                    "not in this machine's keychain, so not moved (set them again): {}",
                    report.missing.join(", ")
                ));
            }
            println!(
                "Keychain copies are kept; remove them with `ikit org secrets forget-keychain` once                  a second machine has read the file."
            );
        }
        SecretsCmd::List => {
            let info = ikit_app::secrets_info(&ctx.paths, &org, &ctx.identity)?;
            if ctx.output == Output::Json {
                return print_json(&info);
            }
            if info.store == "keychain" {
                println!(
                    "Org `{org}` keeps its secrets in this machine's keychain (not shared). Move                      them into an encrypted file with `ikit org secrets init`."
                );
                return Ok(());
            }
            let mut t = table(&["RECIPIENT", "KEY", ""]);
            for r in &info.recipients {
                t.add_row(vec![
                    r.label.clone().unwrap_or_default(),
                    r.key.clone(),
                    if info.me.as_deref() == Some(r.key.as_str()) {
                        "you".to_owned()
                    } else {
                        String::new()
                    },
                ]);
            }
            println!("{t}");
            match info.secrets {
                Some(n) => println!("{n} secret(s) in {}.", info.file.expect("file").display()),
                None => println!("This machine's key cannot read the secrets."),
            }
        }
        SecretsCmd::Grant { key, label } => {
            if ikit_app::secrets_grant(&ctx.paths, &org, &ctx.identity, &key, label)? {
                println!("Granted that key org `{org}`'s secrets.");
            } else {
                println!("That key can already read org `{org}`'s secrets.");
            }
        }
        SecretsCmd::Revoke { key_or_label, yes } => {
            let question = format!(
                "Revoke `{key_or_label}` from org `{org}`'s secrets? It may have kept a copy, so                  rotate what it could read."
            );
            if ctx.confirm(&question, yes)? {
                let removed =
                    ikit_app::secrets_revoke(&ctx.paths, &org, &ctx.identity, &key_or_label)?;
                println!(
                    "Revoked {}{}.",
                    removed.key,
                    removed.label.map(|l| format!(" ({l})")).unwrap_or_default()
                );
            }
        }
        SecretsCmd::ForgetKeychain { yes } => {
            let question = format!(
                "Delete this machine's keychain copies of org `{org}`'s secrets? The encrypted file keeps them."
            );
            if ctx.confirm(&question, yes)? {
                let deleted = ctx
                    .app()
                    .await?
                    .secrets_forget_keychain(&KeyringStore)
                    .await?;
                println!(
                    "Deleted {} keychain entr{}.",
                    deleted.len(),
                    if deleted.len() == 1 { "y" } else { "ies" }
                );
            }
        }
    }
    Ok(())
}

fn render_status(s: &RemoteStatus, secrets_file: bool) {
    let mut t = table(&[]);
    t.add_row(vec!["org".to_owned(), s.org.clone()]);
    t.add_row(vec!["remote".to_owned(), s.remote.clone()]);
    let local = match (s.local_generation, s.synced_at) {
        (Some(g), Some(at)) => format!("generation {g}, synced {}", at.format("%Y-%m-%d %H:%M")),
        _ => "never synced".to_owned(),
    };
    t.add_row(vec!["local".to_owned(), local]);
    t.add_row(vec![
        "local changes".to_owned(),
        if s.local_changes {
            "yes, not pushed".to_owned()
        } else {
            "none".to_owned()
        },
    ]);
    let remote = match &s.head {
        Some(h) => format!(
            "generation {} by {} at {}\n{}",
            h.generation,
            h.written_by,
            h.written_at.format("%Y-%m-%d %H:%M"),
            h.command
        ),
        None => "no HEAD.json".to_owned(),
    };
    t.add_row(vec!["remote head".to_owned(), remote]);
    let lock = match &s.lock {
        Some(l) => format!(
            "{} since {} (expires {})\n{}",
            l.holder,
            l.acquired_at.format("%Y-%m-%d %H:%M"),
            l.expires_at.format("%H:%M"),
            l.command
        ),
        None => "free".to_owned(),
    };
    t.add_row(vec!["lock".to_owned(), lock]);
    t.add_row(vec![
        "secrets".to_owned(),
        if secrets_file {
            "encrypted file, shared (see `ikit org secrets list`)".to_owned()
        } else {
            "this machine's keychain, not shared (`ikit org secrets init` to share)".to_owned()
        },
    ]);
    println!("{t}");
}

fn render(o: &OrgSummary) {
    let mut t = table(&[]);
    t.add_row(vec![
        "org".to_owned(),
        format!("{}{}", o.name, if o.active { " (active)" } else { "" }),
    ]);
    if let Some(d) = &o.display_name {
        t.add_row(vec!["name".to_owned(), d.clone()]);
    }
    t.add_row(vec!["prefix".to_owned(), o.prefix.clone()]);
    t.add_row(vec!["envs".to_owned(), o.envs.join(", ")]);
    if let Some(e) = &o.acme_email {
        t.add_row(vec!["acme email".to_owned(), e.clone()]);
    }
    t.add_row(vec!["workloads".to_owned(), o.workloads.to_string()]);
    t.add_row(vec!["folder".to_owned(), o.dir.display().to_string()]);
    println!("{t}");
}
