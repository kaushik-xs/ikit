use clap::Subcommand;

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum BackupCmd {
    /// Dump a database service of the workload to storage.
    Run {
        service: String,
        /// Storage to write to (default: the only one).
        #[arg(long)]
        storage: Option<String>,
        /// Afterwards keep only the newest N backups of this service.
        #[arg(long)]
        keep: Option<usize>,
    },
    /// List backups (of the selected workload, or --all).
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        service: Option<String>,
        /// Backups of every workload in the org.
        #[arg(long)]
        all: bool,
    },
    /// Restore a backup into its service, replacing the data there.
    Restore {
        id: i64,
        #[arg(long, short)]
        yes: bool,
    },
    /// Delete all but the newest N backups of a service.
    Prune {
        service: String,
        #[arg(long)]
        keep: usize,
        #[arg(long, short)]
        yes: bool,
    },
    /// Print a crontab line that runs this backup on a schedule.
    Schedule {
        service: String,
        /// 5-field cron expression.
        #[arg(long, default_value = "0 2 * * *")]
        cron: String,
        #[arg(long)]
        storage: Option<String>,
        #[arg(long, default_value_t = 7)]
        keep: usize,
    },
}

pub async fn run(ctx: &Ctx, cmd: BackupCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    match cmd {
        BackupCmd::List { service, all } => {
            let wl = if all {
                None
            } else {
                Some(app.resolve_workload_name(ctx.wl.as_deref())?)
            };
            let backups = app.backup_list(wl.as_deref(), service.as_deref()).await?;
            if ctx.output == Output::Json {
                return print_json(&backups);
            }
            if backups.is_empty() {
                println!("No backups. Make one with `ikit backup run <service>`.");
                return Ok(());
            }
            let mut t = table(&[
                "ID", "WORKLOAD", "SERVICE", "STORAGE", "SIZE", "TAKEN", "KEY",
            ]);
            for b in &backups {
                t.add_row(vec![
                    b.id.to_string(),
                    b.workload.clone(),
                    b.service.clone(),
                    b.storage.clone(),
                    human(b.size),
                    b.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
                    b.object_key.clone(),
                ]);
            }
            println!("{t}");
            return Ok(());
        }
        BackupCmd::Restore { id, yes } => {
            let b = app.backup_show(id).await?;
            let question = format!(
                "Restore backup {id} ({} from {}) into {}/{}? Its current data is replaced.",
                human(b.size),
                b.created_at.format("%Y-%m-%d %H:%M"),
                b.workload,
                b.service
            );
            if !ctx.confirm(&question, yes)? {
                return Ok(());
            }
            let b = app.backup_restore(id, &super::deploy::actor()).await?;
            if ctx.output == Output::Json {
                return print_json(&b);
            }
            println!("Restored backup {id} into {}/{}.", b.workload, b.service);
            return Ok(());
        }
        _ => {}
    }
    let wl = app.resolve_workload_name(ctx.wl.as_deref())?;
    match cmd {
        BackupCmd::Run {
            service,
            storage,
            keep,
        } => {
            let report = app
                .backup_run(
                    &wl,
                    &service,
                    storage.as_deref(),
                    keep,
                    &super::deploy::actor(),
                )
                .await?;
            if ctx.output == Output::Json {
                return print_json(&report);
            }
            let b = &report.backup;
            println!(
                "Backup {} of {wl}/{service}: {} to {} ({}).",
                b.id,
                human(b.size),
                b.storage,
                b.object_key
            );
            if !report.pruned.is_empty() {
                let ids: Vec<String> = report.pruned.iter().map(|b| b.id.to_string()).collect();
                println!("Pruned older backups: {}.", ids.join(", "));
            }
        }
        BackupCmd::Prune { service, keep, yes } => {
            if ctx.confirm(
                &format!("Delete all but the newest {keep} backups of {wl}/{service}?"),
                yes,
            )? {
                let pruned = app.backup_prune(&wl, &service, keep).await?;
                if ctx.output == Output::Json {
                    return print_json(&pruned);
                }
                println!("Deleted {} backups.", pruned.len());
            }
        }
        BackupCmd::Schedule {
            service,
            cron,
            storage,
            keep,
        } => {
            let storage = match storage {
                Some(s) => s,
                None => {
                    let mut all = app.storage_list().await?;
                    if all.len() != 1 {
                        anyhow::bail!("pass --storage <name>");
                    }
                    all.remove(0).name
                }
            };
            let line = app.backup_cron_line(&wl, &service, &storage, &cron, keep)?;
            println!("{line}");
            eprintln!("Add this line with `crontab -e` (ikit does not change your crontab).");
        }
        BackupCmd::List { .. } | BackupCmd::Restore { .. } => unreachable!("handled above"),
    }
    Ok(())
}

fn human(bytes: i64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", b / 1024.0 / 1024.0)
    } else if b >= 1024.0 {
        format!("{:.1} KiB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
