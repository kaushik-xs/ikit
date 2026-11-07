//! Database backups: dump inside the service's container, compress with zstd, upload to a
//! storage, and keep a catalog (with checksums) in the org's state database.
//!
//! Each database in the service is dumped to its own object and catalogued separately, so a
//! Postgres holding one database per app backs up all of them. Postgres uses `pg_dump -Fc` /
//! `pg_restore --clean`; MySQL uses `mysqldump --databases` / `mysql`. Dumps are held in memory,
//! which suits databases up to a few GB.

use std::time::Duration;

use ikit_core::{Backup, RunAction, RunResult, SqlEngine, WorkloadStatus};
use ikit_state::{NewBackup, NewRun};
use serde::Serialize;

use crate::hooks_sql::checksum_bytes;
use crate::{App, AppError, Result};

const DUMP_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Set in the service's container to the database being dumped or restored, so its name
/// never passes through a shell.
const DB_VAR: &str = "IKIT_DB";

/// MySQL's own schemas, never backed up.
const MYSQL_SYSTEM: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];

#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    /// One per database, in name order.
    pub backups: Vec<Backup>,
    /// Older backups deleted by `keep`.
    pub pruned: Vec<Backup>,
}

/// Prints the service's databases, one per line.
fn list_command(engine: SqlEngine) -> &'static str {
    match engine {
        SqlEngine::Postgres => {
            r#"exec psql -X -A -t -U "${POSTGRES_USER:-postgres}" -d "${POSTGRES_DB:-${POSTGRES_USER:-postgres}}" -c "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname""#
        }
        SqlEngine::Mysql => {
            r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -uroot -N -B -e "SHOW DATABASES""#
        }
    }
}

/// Dumps the database named by `$IKIT_DB`.
fn dump_command(engine: SqlEngine) -> &'static str {
    match engine {
        SqlEngine::Postgres => r#"exec pg_dump -Fc -U "${POSTGRES_USER:-postgres}" -d "$IKIT_DB""#,
        SqlEngine::Mysql => {
            r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysqldump -uroot --single-transaction --routines --triggers --databases "$IKIT_DB""#
        }
    }
}

/// Restores into the database named by `$IKIT_DB`, creating it if it is missing. With no
/// database (backups from before per-database dumps), restores into the default database.
fn restore_command(engine: SqlEngine, database: bool) -> &'static str {
    match (engine, database) {
        (SqlEngine::Postgres, true) => {
            r#"createdb -U "${POSTGRES_USER:-postgres}" "$IKIT_DB" 2>/dev/null; exec pg_restore --clean --if-exists --no-owner -U "${POSTGRES_USER:-postgres}" -d "$IKIT_DB""#
        }
        (SqlEngine::Postgres, false) => {
            r#"exec pg_restore --clean --if-exists --no-owner -U "${POSTGRES_USER:-postgres}" -d "${POSTGRES_DB:-${POSTGRES_USER:-postgres}}""#
        }
        // A `--databases` dump creates and selects its database itself.
        (SqlEngine::Mysql, true) => r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -uroot"#,
        (SqlEngine::Mysql, false) => {
            r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -uroot "$MYSQL_DATABASE""#
        }
    }
}

/// The databases in `list_command`'s output worth backing up.
fn databases(engine: SqlEngine, listed: &str) -> Vec<String> {
    listed
        .lines()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .filter(|d| engine != SqlEngine::Mysql || !MYSQL_SYSTEM.contains(d))
        .map(str::to_owned)
        .collect()
}

impl App {
    async fn backup_target(
        &self,
        workload: &str,
        service: &str,
    ) -> Result<(ikit_core::Workload, SqlEngine)> {
        let wl = self.workload_get(workload).await?;
        if wl.status != WorkloadStatus::Applied {
            return Err(AppError::Invalid(format!(
                "{workload} is {}; backups need the deployed database",
                wl.status
            )));
        }
        let (instance, _) = self.service_show(workload, service).await?;
        let engine = SqlEngine::detect(&instance.registry_service).ok_or_else(|| {
            AppError::Invalid(format!(
                "`{service}` ({}) is not a database ikit can back up (postgres, mysql)",
                instance.registry_service
            ))
        })?;
        Ok((wl, engine))
    }

    /// Dumps each database of `service` to `storage` (or the only storage), one backup per
    /// database. With `keep`, deletes all but the newest `keep` backups of each database
    /// afterwards. Stops at the first database that fails; the ones before it stay catalogued.
    pub async fn backup_run(
        &self,
        workload: &str,
        service: &str,
        storage: Option<&str>,
        keep: Option<usize>,
        actor: &str,
    ) -> Result<BackupReport> {
        let (wl, engine) = self.backup_target(workload, service).await?;
        let storage = self.storage_or_default(storage).await?;
        let target = self.target_show(&wl.target).await?;
        let place = self.place(&target, workload)?;
        let plan = serde_json::json!({ "service": service, "storage": storage.name }).to_string();
        let run = NewRun {
            action: RunAction::Backup,
            actor,
            plan_json: &plan,
            compose_yaml: None,
        };
        let run_id = self.store.workloads().start_run(wl.id, run).await?;

        let result = async {
            let args = ["exec", "-T", service, "sh", "-c", list_command(engine)];
            let out = self
                .compose_with(&place, workload, &args, None, DUMP_TIMEOUT)
                .await?;
            if !out.success() {
                return Err(AppError::Invalid(format!(
                    "listing the databases in `{service}` failed: {}",
                    out.failure_detail()
                )));
            }
            let dbs = databases(engine, &out.stdout);
            if dbs.is_empty() {
                return Err(AppError::Invalid(format!(
                    "`{service}` has no databases to back up"
                )));
            }
            let ext = match engine {
                SqlEngine::Postgres => "pgdump",
                SqlEngine::Mysql => "sql",
            };
            let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
            let mut backups = Vec::with_capacity(dbs.len());
            for db in &dbs {
                let env = format!("{DB_VAR}={db}");
                let args = [
                    "exec",
                    "-T",
                    "-e",
                    env.as_str(),
                    service,
                    "sh",
                    "-c",
                    dump_command(engine),
                ];
                let out = self
                    .compose_with(&place, workload, &args, None, DUMP_TIMEOUT)
                    .await?;
                if !out.success() || out.raw_stdout.is_empty() {
                    return Err(AppError::Invalid(format!(
                        "dumping database `{db}` of `{service}` failed: {}",
                        out.failure_detail()
                    )));
                }
                let compressed = zstd::encode_all(out.raw_stdout.as_slice(), 3)?;
                let sha256 = checksum_bytes(&compressed);
                let key = storage.key(&format!(
                    "{}/{workload}/{service}/{db}-{stamp}.{ext}.zst",
                    self.org.name
                ));
                let size = compressed.len() as i64;
                self.put_object(&storage, &key, compressed).await?;
                let new = NewBackup {
                    workload,
                    service,
                    database: Some(db),
                    engine,
                    storage: &storage.name,
                    object_key: &key,
                    size,
                    sha256: &sha256,
                };
                backups.push(self.store.storage().add_backup(new).await?);
            }
            let pruned = match keep {
                Some(n) => self.backup_prune(workload, service, n).await?,
                None => vec![],
            };
            Ok(BackupReport { backups, pruned })
        }
        .await;
        let (status, error) = match &result {
            Ok(_) => (RunResult::Ok, None),
            Err(e) => (RunResult::Failed, Some(e.to_string())),
        };
        self.store
            .workloads()
            .finish_run(run_id, status, error.as_deref())
            .await?;
        result
    }

    pub async fn backup_list(
        &self,
        workload: Option<&str>,
        service: Option<&str>,
    ) -> Result<Vec<Backup>> {
        Ok(self.store.storage().backups(workload, service).await?)
    }

    pub async fn backup_show(&self, id: i64) -> Result<Backup> {
        self.store
            .storage()
            .backup(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("backup {id}")))
    }

    /// Restores backup `id` into its service, replacing the data there. The checksum is
    /// verified first.
    pub async fn backup_restore(&self, id: i64, actor: &str) -> Result<Backup> {
        let backup = self.backup_show(id).await?;
        let (wl, engine) = self
            .backup_target(&backup.workload, &backup.service)
            .await?;
        if engine != backup.engine {
            return Err(AppError::Invalid(format!(
                "backup {id} is a {} dump but `{}` is now {engine}",
                backup.engine, backup.service
            )));
        }
        let storage = self.storage_show(&backup.storage).await?;
        let target = self.target_show(&wl.target).await?;
        let place = self.place(&target, &wl.name)?;
        let holder = format!("{actor} (pid {})", std::process::id());
        self.lock(&wl, &holder).await?;
        let result = async {
            let plan = serde_json::json!({ "backup": id, "service": backup.service }).to_string();
            let run = NewRun {
                action: RunAction::Restore,
                actor,
                plan_json: &plan,
                compose_yaml: None,
            };
            let run_id = self.store.workloads().start_run(wl.id, run).await?;
            let outcome = async {
                let compressed = self.get_object(&storage, &backup.object_key).await?;
                if checksum_bytes(&compressed) != backup.sha256 {
                    return Err(AppError::Invalid(format!(
                        "backup {id} does not match its checksum; not restoring"
                    )));
                }
                let dump = zstd::decode_all(compressed.as_slice())?;
                let env = backup.database.as_ref().map(|db| format!("{DB_VAR}={db}"));
                let mut args = vec!["exec", "-T"];
                if let Some(env) = &env {
                    args.extend(["-e", env.as_str()]);
                }
                args.extend([
                    backup.service.as_str(),
                    "sh",
                    "-c",
                    restore_command(engine, env.is_some()),
                ]);
                let out = self
                    .compose_with(&place, &wl.name, &args, Some(&dump), DUMP_TIMEOUT)
                    .await?;
                if !out.success() {
                    return Err(AppError::Invalid(format!(
                        "restoring backup {id} into `{}` failed: {}",
                        backup.service,
                        crate::hooks_sql::sql_error(&out)
                    )));
                }
                Ok(())
            }
            .await;
            let (status, error) = match &outcome {
                Ok(()) => (RunResult::Ok, None),
                Err(e) => (RunResult::Failed, Some(e.to_string())),
            };
            self.store
                .workloads()
                .finish_run(run_id, status, error.as_deref())
                .await?;
            outcome
        }
        .await;
        self.store.workloads().unlock(wl.id, &holder).await?;
        result.map(|()| backup)
    }

    /// Deletes all but the newest `keep` backups of each database of a service, objects first.
    pub async fn backup_prune(
        &self,
        workload: &str,
        service: &str,
        keep: usize,
    ) -> Result<Vec<Backup>> {
        let all = self
            .store
            .storage()
            .backups(Some(workload), Some(service))
            .await?;
        let mut seen: std::collections::HashMap<Option<String>, usize> = Default::default();
        let mut pruned = Vec::new();
        for b in all {
            let n = seen.entry(b.database.clone()).or_default();
            *n += 1;
            if *n <= keep {
                continue;
            }
            let storage = self.storage_show(&b.storage).await?;
            self.delete_object(&storage, &b.object_key).await?;
            self.store.storage().delete_backup(b.id).await?;
            pruned.push(b);
        }
        Ok(pruned)
    }

    /// A crontab line that runs this backup on a schedule.
    pub fn backup_cron_line(
        &self,
        workload: &str,
        service: &str,
        storage: &str,
        cron: &str,
        keep: usize,
    ) -> Result<String> {
        if cron.split_whitespace().count() != 5 {
            return Err(AppError::Invalid(format!(
                "`{cron}` is not a 5-field cron expression (e.g. \"0 2 * * *\")"
            )));
        }
        let exe = std::env::current_exe()?;
        Ok(format!(
            "{cron} {} --home {} --org {} --wl {workload} backup run {service} --storage {storage} --keep {keep} --no-input",
            exe.display(),
            self.paths.home.display(),
            self.org.name
        ))
    }
}
