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
    /// List the remote's generations, newest first.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Make an old generation current again, as a new generation; history is kept.
    Restore {
        generation: u64,
        /// Also bring back that generation's secrets and who could read them. Off by
        /// default: an old snapshot may still let revoked keys in.
        #[arg(long)]
        with_secrets: bool,
        #[arg(long, short)]
        yes: bool,
    },
    /// Push local changes that a failed command left unpushed (see `ikit org status`).
    Push,
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
        /// Whose key it is, e.g. alice@rg2k. Label a recovery key `recovery` and automation
        /// keys `ci-…` so `check` can tell them from people.
        #[arg(long = "as", value_name = "LABEL")]
        label: Option<String>,
        /// Grant only an environment's own secrets file (see `split`).
        #[arg(long)]
        env: Option<String>,
    },
    /// Stop a key (or label) from reading the secrets. Rotate what it could read: it may
    /// have kept a copy.
    Revoke {
        key_or_label: String,
        /// Revoke from an environment's own secrets file instead of the org's.
        #[arg(long)]
        env: Option<String>,
        #[arg(long, short)]
        yes: bool,
    },
    /// Give an environment its own secrets file for a smaller group: its workloads' and hooks'
    /// secrets move there, readable by your key and each --recipient.
    Split {
        env: String,
        #[arg(long = "recipient", value_name = "KEY")]
        recipients: Vec<String>,
    },
    /// Move an environment's secrets back into the org file; everyone who reads the org
    /// file can then read them.
    Merge {
        env: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// Fail if a secrets file has no recovery key or too few people to read it.
    Check {
        /// People (keys not labelled recovery… or ci…) each file needs.
        #[arg(long, default_value_t = 2)]
        min_people: usize,
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
            for n in &report.notices {
                crate::warn(n);
            }
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
        OrgCmd::History { limit } => {
            let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
            let history = ikit_app::org_history(&ctx.paths, &org, limit).await?;
            if ctx.output == Output::Json {
                return print_json(&history);
            }
            let mut t = table(&["GEN", "WHEN", "BY", "COMMAND"]);
            for h in &history {
                t.add_row(vec![
                    format!("{}{}", h.generation, if h.current { " *" } else { "" }),
                    h.written_at.format("%Y-%m-%d %H:%M").to_string(),
                    h.written_by.clone().unwrap_or_default(),
                    h.command
                        .clone()
                        .unwrap_or_else(|| "(no record)".to_owned()),
                ]);
            }
            println!("{t}");
        }
        OrgCmd::Restore {
            generation,
            with_secrets,
            yes,
        } => {
            let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
            let what = if with_secrets {
                "its state, secrets and who can read them"
            } else {
                "its state (the current secrets and readers are kept)"
            };
            let question =
                format!("Make generation {generation} of org `{org}` current again: {what}?");
            if ctx.confirm(&question, yes)? {
                let report =
                    ikit_app::org_restore(&ctx.paths, &org, generation, with_secrets).await?;
                if ctx.output == Output::Json {
                    return print_json(&report);
                }
                println!("Restored generation {generation} of org `{org}`.");
            }
        }
        OrgCmd::Push => {
            // The command's write session pushes whatever is unpushed.
            let org = ikit_app::resolve_org(&ctx.paths, ctx.org.as_deref())?;
            if ikit_app::remote_spec(&ctx.paths, &org)?.is_none() {
                anyhow::bail!("org `{org}` has no remote");
            }
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
                "Keychain copies are kept; remove them with `ikit org secrets forget-keychain` once \
                 a second machine has read the file."
            );
        }
        SecretsCmd::List => {
            let info = ikit_app::secrets_info(&ctx.paths, &org, &ctx.identity)?;
            if ctx.output == Output::Json {
                return print_json(&info);
            }
            if info.store == "keychain" {
                println!(
                    "Org `{org}` keeps its secrets in this machine's keychain (not shared). Move \
                     them into an encrypted file with `ikit org secrets init`."
                );
                return Ok(());
            }
            let mut t = table(&["FILE", "RECIPIENT", "KEY", "SECRETS"]);
            for scope in &info.scopes {
                for (i, r) in scope.recipients.iter().enumerate() {
                    let you = info.me.as_deref() == Some(r.key.as_str());
                    t.add_row(vec![
                        if i == 0 {
                            format!("{}.age", scope.scope)
                        } else {
                            String::new()
                        },
                        format!(
                            "{}{}",
                            r.label.clone().unwrap_or_default(),
                            if you { " (you)" } else { "" }
                        ),
                        r.key.clone(),
                        match (i, scope.secrets) {
                            (0, Some(n)) => n.to_string(),
                            (0, None) => "you cannot read it".to_owned(),
                            _ => String::new(),
                        },
                    ]);
                }
            }
            println!("{t}");
        }
        SecretsCmd::Grant { key, label, env } => {
            let which = scope_words(&org, env.as_deref());
            if ikit_app::secrets_grant(
                &ctx.paths,
                &org,
                &ctx.identity,
                &key,
                label,
                env.as_deref(),
            )? {
                println!("Granted that key {which}.");
            } else {
                println!("That key can already read {which}.");
            }
        }
        SecretsCmd::Revoke {
            key_or_label,
            env,
            yes,
        } => {
            let question = format!(
                "Revoke `{key_or_label}` from {}? It may have kept a copy, so rotate what it \
                 could read.",
                scope_words(&org, env.as_deref())
            );
            if ctx.confirm(&question, yes)? {
                let removed = ikit_app::secrets_revoke(
                    &ctx.paths,
                    &org,
                    &ctx.identity,
                    &key_or_label,
                    env.as_deref(),
                )?;
                println!(
                    "Revoked {}{}.",
                    removed.key,
                    removed.label.map(|l| format!(" ({l})")).unwrap_or_default()
                );
            }
        }
        SecretsCmd::Split { env, recipients } => {
            let extra = recipients
                .iter()
                .map(|k| ikit_app::SecretsRecipient::parse(k))
                .collect::<Result<Vec<_>, _>>()?;
            let moved = ikit_app::secrets_split(&ctx.paths, &org, &ctx.identity, &env, extra)?;
            println!(
                "Org `{org}`'s {env} secrets now have their own file; moved {} secret(s). Grant \
                 more readers with `ikit org secrets grant <key> --env {env}`.",
                moved.len()
            );
        }
        SecretsCmd::Merge { env, yes } => {
            let question = format!(
                "Move org `{org}`'s {env} secrets into the org file, readable by everyone who \
                 reads that?"
            );
            if ctx.confirm(&question, yes)? {
                let moved = ikit_app::secrets_merge(&ctx.paths, &org, &ctx.identity, &env)?;
                println!("Moved {} {env} secret(s) into the org file.", moved.len());
            }
        }
        SecretsCmd::Check { min_people } => {
            let problems = ikit_app::secrets_check(&ctx.paths, &org, min_people)?;
            if ctx.output == Output::Json {
                print_json(&problems)?;
            } else if problems.is_empty() {
                println!(
                    "Org `{org}`'s secrets files each have a recovery key and {min_people}+ people."
                );
            } else {
                for p in &problems {
                    println!("{}.age: {}", p.scope, p.problem);
                }
            }
            if !problems.is_empty() {
                std::process::exit(1);
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

/// "org `x`'s secrets" or "org `x`'s prod secrets".
fn scope_words(org: &str, env: Option<&str>) -> String {
    match env {
        Some(env) => format!("org `{org}`'s {env} secrets"),
        None => format!("org `{org}`'s secrets"),
    }
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
