use anyhow::bail;
use clap::Subcommand;
use ikit_app::WorkloadDetail;
use inquire::Select;

use crate::output::{Output, print_json, table};
use crate::{Ctx, warn};

#[derive(Debug, Subcommand)]
pub enum WorkloadCmd {
    /// Create wl-<env>-<nnn> on a target and select it.
    Create {
        /// Environment from config.yaml (dev, qa, stg, prod by default).
        #[arg(long)]
        env: Option<String>,
        /// Target to run on (see `ikit target list`).
        #[arg(long)]
        target: Option<String>,
    },
    /// List workloads; * marks the selected one.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        env: Option<String>,
        /// List workloads of every org.
        #[arg(long)]
        all_orgs: bool,
    },
    /// Show a workload and its services (default: the selected one).
    Show { name: Option<String> },
    /// Show the workload's apply and destroy history.
    Runs {
        name: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Select the workload later commands act on; switches to its org if needed.
    Use { name: String },
    /// Remove a workload definition and its keychain secrets. Nothing is deployed yet, so
    /// nothing is torn down.
    #[command(alias = "remove")]
    Rm {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(ctx: &Ctx, cmd: WorkloadCmd) -> anyhow::Result<()> {
    match &cmd {
        WorkloadCmd::Use { name } => return use_workload(ctx, name).await,
        WorkloadCmd::List {
            env,
            all_orgs: true,
        } => return list_all_orgs(ctx, env.as_deref()).await,
        _ => {}
    }
    let app = ctx.app().await?;
    match cmd {
        WorkloadCmd::Create { env, target } => {
            let env = match env {
                Some(env) => env,
                None if ctx.interactive => {
                    Select::new("Environment:", app.config()?.envs).prompt()?
                }
                None => bail!("--env is required"),
            };
            let target = match target {
                Some(t) => t,
                None => {
                    let targets: Vec<String> = app
                        .target_list()
                        .await?
                        .into_iter()
                        .map(|t| t.name)
                        .collect();
                    if targets.is_empty() {
                        bail!(
                            "no targets yet; attach one with `ikit target attach vm <name> --local`"
                        );
                    }
                    if !ctx.interactive {
                        bail!("--target is required");
                    }
                    Select::new("Target:", targets).prompt()?
                }
            };
            let target_status = app.target_show(&target).await?.status;
            let workload = app.workload_create(&env, &target).await?;
            if ctx.output == Output::Json {
                return print_json(&workload);
            }
            println!(
                "Created {} on {} and selected it.",
                workload.name, workload.target
            );
            if target_status != ikit_core::TargetStatus::Ready {
                warn(format!(
                    "target {target} is {target_status}; run `ikit target test {target}`"
                ));
            }
            println!("Next: add services with `ikit sg add <app|platform|data>`.");
        }
        WorkloadCmd::List { env, .. } => {
            let workloads = app.workload_list(env.as_deref()).await?;
            if ctx.output == Output::Json {
                return print_json(&workloads);
            }
            if workloads.is_empty() {
                println!(
                    "No workloads. Create one with `ikit wl create --env dev --target <target>`."
                );
                return Ok(());
            }
            let current = app.current_workload()?;
            let mut t = table(&["", "NAME", "ENV", "TARGET", "STATUS", "SERVICES", "UPDATED"]);
            for w in &workloads {
                let services = app.services_list(&w.name).await?.len();
                t.add_row(vec![
                    if current.as_deref() == Some(w.name.as_str()) {
                        "*"
                    } else {
                        ""
                    }
                    .to_owned(),
                    w.name.clone(),
                    w.env.clone(),
                    format!("{} ({})", w.target, w.target_kind),
                    w.status.to_string(),
                    services.to_string(),
                    w.updated_at.format("%Y-%m-%d %H:%M").to_string(),
                ]);
            }
            println!("{t}");
        }
        WorkloadCmd::Show { name } => {
            let name = app.resolve_workload_name(name.as_deref().or(ctx.wl.as_deref()))?;
            let detail = app.workload_show(&name).await?;
            render(ctx, &detail)?;
            if ctx.output == Output::Table {
                for u in app.workload_unmet_dependencies(&name).await? {
                    warn(format!(
                        "{} depends on {}, which no service in {name} provides",
                        u.service, u.dependency
                    ));
                }
            }
        }
        WorkloadCmd::Use { .. } => unreachable!("handled above"),
        WorkloadCmd::Runs { name, limit } => {
            let ctx = Ctx {
                wl: name.or_else(|| ctx.wl.clone()),
                ..ctx.clone()
            };
            return super::deploy::runs(&ctx, limit).await;
        }
        WorkloadCmd::Rm { name, yes } => {
            if ctx.confirm(
                &format!("Remove workload {name}, its services and their keychain secrets?"),
                yes,
            )? {
                app.workload_remove(&name).await?;
                if ctx.output == Output::Table {
                    println!("Removed {name}.");
                }
            }
        }
    }
    Ok(())
}

/// Selects `name`, in the org given by `--org`, else the org whose prefix matches, else the
/// active org. Makes that org active too.
async fn use_workload(ctx: &Ctx, name: &str) -> anyhow::Result<()> {
    let org = match &ctx.org {
        Some(org) => org.clone(),
        None => match ikit_app::find_org_for_workload(&ctx.paths, name)? {
            Some(org) => org,
            None => ikit_app::resolve_org(&ctx.paths, None)?,
        },
    };
    let app = ikit_app::App::open(
        &ctx.paths,
        &org,
        std::sync::Arc::new(ikit_app::KeyringStore),
    )
    .await?;
    let workload = app.workload_use(name).await?;
    ikit_app::org_use(&ctx.paths, &org)?;
    if ctx.output == Output::Json {
        return print_json(&workload);
    }
    println!("Now using {} (org `{org}`).", workload.name);
    Ok(())
}

async fn list_all_orgs(ctx: &Ctx, env: Option<&str>) -> anyhow::Result<()> {
    let mut rows = Vec::new();
    for org in ikit_app::org_list(&ctx.paths).await? {
        let app = ikit_app::App::open(
            &ctx.paths,
            &org.name,
            std::sync::Arc::new(ikit_app::KeyringStore),
        )
        .await?;
        for w in app.workload_list(env).await? {
            rows.push((org.name.clone(), w));
        }
    }
    if ctx.output == Output::Json {
        let json: Vec<_> = rows
            .iter()
            .map(|(org, w)| serde_json::json!({ "org": org, "workload": w }))
            .collect();
        return print_json(&json);
    }
    if rows.is_empty() {
        println!("No workloads in any org.");
        return Ok(());
    }
    let mut t = table(&["ORG", "NAME", "ENV", "TARGET", "STATUS", "UPDATED"]);
    for (org, w) in &rows {
        t.add_row(vec![
            org.clone(),
            w.name.clone(),
            w.env.clone(),
            format!("{} ({})", w.target, w.target_kind),
            w.status.to_string(),
            w.updated_at.format("%Y-%m-%d %H:%M").to_string(),
        ]);
    }
    println!("{t}");
    Ok(())
}

pub fn render(ctx: &Ctx, d: &WorkloadDetail) -> anyhow::Result<()> {
    if ctx.output == Output::Json {
        return print_json(d);
    }
    let w = &d.workload;
    let mut t = table(&[]);
    t.add_row(vec!["name".to_owned(), w.name.clone()]);
    t.add_row(vec!["org".to_owned(), d.org.clone()]);
    t.add_row(vec!["env".to_owned(), w.env.clone()]);
    t.add_row(vec![
        "target".to_owned(),
        format!("{} ({})", w.target, w.target_kind),
    ]);
    t.add_row(vec!["status".to_owned(), w.status.to_string()]);
    t.add_row(vec!["spec".to_owned(), d.spec_file.display().to_string()]);
    println!("{t}");
    if d.services.is_empty() {
        println!("No services yet. Add some with `ikit sg add <app|platform|data>`.");
        return Ok(());
    }
    super::service::print_table(&d.services);
    Ok(())
}
