use clap::Subcommand;
use ikit_app::{KeyringStore, NewOrg, OrgSummary};

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
    }
    Ok(())
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
    t.add_row(vec!["workloads".to_owned(), o.workloads.to_string()]);
    t.add_row(vec!["folder".to_owned(), o.dir.display().to_string()]);
    println!("{t}");
}
