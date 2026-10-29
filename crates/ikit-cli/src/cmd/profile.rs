use std::collections::BTreeMap;

use anyhow::{Context, bail};
use clap::Subcommand;
use ikit_app::AddProfile;
use ikit_core::{FieldSpec, Profile, ProfileKind, Requirement};
use inquire::{Confirm, Password, PasswordDisplayMode, Text};

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum ProfileCmd {
    /// Add a profile. Missing fields are prompted for; secrets can also come from
    /// IKIT_SECRET_<FIELD> environment variables (e.g. IKIT_SECRET_API_TOKEN).
    Add {
        /// aws | azure | gcp | zerotier | k8s
        kind: ProfileKind,
        name: String,
        /// Non-secret field value, e.g. --set region=eu-west-1 (repeatable).
        #[arg(long = "set", value_name = "KEY=VALUE", value_parser = parse_key_value)]
        set: Vec<(String, String)>,
    },
    /// Change fields of an existing profile. Secret fields are prompted for with --secret
    /// (or read from IKIT_SECRET_<FIELD>).
    Set {
        kind: ProfileKind,
        name: String,
        /// Non-secret field value, e.g. --set region=eu-central-1 (repeatable).
        #[arg(long = "set", value_name = "KEY=VALUE", value_parser = parse_key_value)]
        set: Vec<(String, String)>,
        /// Secret field to set (repeatable).
        #[arg(long = "secret", value_name = "FIELD")]
        secrets: Vec<String>,
        /// Field to remove (repeatable).
        #[arg(long = "unset", value_name = "FIELD")]
        unset: Vec<String>,
    },
    /// List profiles.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        kind: Option<ProfileKind>,
    },
    /// Show one profile. Secret values are never printed.
    Show { kind: ProfileKind, name: String },
    /// Remove a profile and its keychain secrets.
    #[command(alias = "remove")]
    Rm {
        kind: ProfileKind,
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// List the fields a profile kind accepts.
    Fields { kind: ProfileKind },
}

pub async fn run(ctx: &Ctx, cmd: ProfileCmd) -> anyhow::Result<()> {
    if let ProfileCmd::Fields { kind } = cmd {
        return fields(ctx, kind);
    }

    let app = ctx.app().await?;
    match cmd {
        ProfileCmd::Add { kind, name, set } => {
            let values = collect_values(ctx, kind, set)?;
            let profile = app.profile_add(AddProfile { kind, name, values }).await?;
            render_profile(ctx, &profile)
        }
        ProfileCmd::Set {
            kind,
            name,
            set,
            secrets,
            unset,
        } => {
            if set.is_empty() && secrets.is_empty() && unset.is_empty() {
                bail!("nothing to change; pass --set, --secret or --unset");
            }
            let mut values = BTreeMap::new();
            for (key, value) in set {
                match kind.field(&key) {
                    None => bail!(
                        "`{key}` is not a field of {kind} profiles (see `ikit profile fields {kind}`)"
                    ),
                    Some(f) if f.secret => bail!("`{key}` is a secret; use --secret {key}"),
                    Some(_) => values.insert(key, value),
                };
            }
            for key in secrets {
                let field = kind.field(&key).filter(|f| f.secret).ok_or_else(|| {
                    anyhow::anyhow!("`{key}` is not a secret field of {kind} profiles")
                })?;
                let value = match std::env::var(secret_env_var(&key)) {
                    Ok(v) => v,
                    Err(_) if ctx.interactive => prompt(field, true)?.unwrap_or_default(),
                    Err(_) => bail!("set {} or run interactively", secret_env_var(&key)),
                };
                values.insert(key, value);
            }
            let profile = app.profile_update(kind, &name, values, &unset).await?;
            render_profile(ctx, &profile)
        }
        ProfileCmd::List { kind } => {
            let profiles = app.profile_list(kind).await?;
            if ctx.output == Output::Json {
                return print_json(&profiles);
            }
            if profiles.is_empty() {
                println!("No profiles. Add one with `ikit profile add <kind> <name>`.");
                return Ok(());
            }
            let mut t = table(&["KIND", "NAME", "CONFIG", "SECRETS", "UPDATED"]);
            for p in &profiles {
                let config: Vec<String> =
                    p.config.iter().map(|(k, v)| format!("{k}={v}")).collect();
                t.add_row(vec![
                    p.kind.to_string(),
                    p.name.clone(),
                    config.join("\n"),
                    p.secret_keys.join("\n"),
                    p.updated_at.format("%Y-%m-%d %H:%M").to_string(),
                ]);
            }
            println!("{t}");
            Ok(())
        }
        ProfileCmd::Show { kind, name } => {
            render_profile(ctx, &app.profile_show(kind, &name).await?)
        }
        ProfileCmd::Rm { kind, name, yes } => {
            if !yes {
                if !ctx.interactive {
                    bail!("refusing to remove {kind}/{name} without confirmation; pass --yes");
                }
                let confirmed = Confirm::new(&format!(
                    "Remove profile {kind}/{name} and its keychain secrets?"
                ))
                .with_default(false)
                .prompt()?;
                if !confirmed {
                    return Ok(());
                }
            }
            app.profile_remove(kind, &name).await?;
            if ctx.output == Output::Table {
                println!("Removed profile {kind}/{name}.");
            }
            Ok(())
        }
        ProfileCmd::Fields { .. } => unreachable!("handled above"),
    }
}

/// Merges `--set` values, `IKIT_SECRET_*` env vars and prompts. Optional fields are only
/// prompted for when no `--set` was given, so scripted calls are not interrupted.
fn collect_values(
    ctx: &Ctx,
    kind: ProfileKind,
    set: Vec<(String, String)>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let prompt_optional = ctx.interactive && set.is_empty();
    let mut values = BTreeMap::new();
    for (key, value) in set {
        if kind.field(&key).is_some_and(|f| f.secret) {
            bail!(
                "`{key}` is a secret; pass it via the prompt or {} instead of --set",
                secret_env_var(&key)
            );
        }
        values.insert(key, value);
    }

    for field in kind.fields() {
        if values.contains_key(field.key) {
            continue;
        }
        if field.secret
            && let Ok(v) = std::env::var(secret_env_var(field.key))
        {
            values.insert(field.key.to_owned(), v);
            continue;
        }
        let required = field.is_required(&values);
        if !ctx.interactive || !(required || prompt_optional) {
            continue;
        }
        if let Some(v) = prompt(field, required)? {
            values.insert(field.key.to_owned(), v);
        }
    }
    Ok(values)
}

fn prompt(field: &FieldSpec, required: bool) -> anyhow::Result<Option<String>> {
    let label = format!("{}:", field.key);
    let help = if required {
        field.description.to_owned()
    } else {
        format!("{} (optional, Enter to skip)", field.description)
    };
    let value = if field.secret {
        Password::new(&label)
            .with_help_message(&help)
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .prompt()?
    } else {
        let mut text = Text::new(&label).with_help_message(&help);
        if let Some(default) = field.default {
            text = text.with_default(default);
        }
        text.prompt()?
    };
    Ok(Some(value).filter(|v| !v.trim().is_empty()))
}

fn secret_env_var(key: &str) -> String {
    format!("IKIT_SECRET_{}", key.to_ascii_uppercase())
}

fn render_profile(ctx: &Ctx, p: &Profile) -> anyhow::Result<()> {
    if ctx.output == Output::Json {
        return print_json(p);
    }
    let mut t = table(&["FIELD", "VALUE"]);
    t.add_row(vec!["profile".to_owned(), format!("{}/{}", p.kind, p.name)]);
    for (k, v) in &p.config {
        t.add_row(vec![k.clone(), v.clone()]);
    }
    for k in &p.secret_keys {
        t.add_row(vec![k.clone(), "•••••• (keychain)".to_owned()]);
    }
    t.add_row(vec!["created".to_owned(), p.created_at.to_rfc3339()]);
    t.add_row(vec!["updated".to_owned(), p.updated_at.to_rfc3339()]);
    println!("{t}");
    Ok(())
}

fn fields(ctx: &Ctx, kind: ProfileKind) -> anyhow::Result<()> {
    if ctx.output == Output::Json {
        let fields: Vec<_> = kind
            .fields()
            .iter()
            .map(|f| {
                serde_json::json!({
                    "key": f.key,
                    "description": f.description,
                    "required": requirement(f),
                    "secret": f.secret,
                    "default": f.default,
                })
            })
            .collect();
        return print_json(&fields);
    }
    let mut t = table(&["FIELD", "REQUIRED", "SECRET", "DESCRIPTION"]);
    for f in kind.fields() {
        let mut description = f.description.to_owned();
        if let Some(d) = f.default {
            description.push_str(&format!(" [default: {d}]"));
        }
        t.add_row(vec![
            f.key.to_owned(),
            requirement(f),
            if f.secret { "yes" } else { "" }.to_owned(),
            description,
        ]);
    }
    println!("{t}");
    Ok(())
}

fn requirement(f: &FieldSpec) -> String {
    match f.requirement {
        Requirement::Required => "yes".to_owned(),
        Requirement::Optional => "".to_owned(),
        Requirement::RequiredWith(other) => format!("with {other}"),
    }
}

fn parse_key_value(s: &str) -> anyhow::Result<(String, String)> {
    let (k, v) = s.split_once('=').context("expected KEY=VALUE")?;
    Ok((k.trim().to_owned(), v.to_owned()))
}
