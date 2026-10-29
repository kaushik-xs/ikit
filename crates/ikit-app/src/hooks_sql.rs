//! SQL hooks: scripts, the target-side ledger, and the two ways SQL reaches the database.
//!
//! - `exec` pipes SQL through `docker compose exec` into the container's own client (`psql`,
//!   `mysql`) with the container's credentials, so it needs no published ports.
//! - `direct` connects from the machine running ikit to the service's published port, using
//!   the credentials ikit holds for the service.
//!
//! Either way each script commits together with its row in `_ikit_hook_ledger` in the target
//! database. That ledger decides what has run, so a wiped or restored database re-runs what
//! it no longer has.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use ikit_core::hook::is_script_name;
use ikit_core::{Connection, SqlAction, SqlEngine};
use sha2::{Digest, Sha256};
use sqlx::Connection as _;

use crate::deploy::{Place, sh};
use crate::{App, AppError, ExecOutput, Result};

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const LEDGER_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct Script {
    pub name: String,
    pub checksum: String,
    pub sql: String,
}

pub(crate) fn checksum(sql: &str) -> String {
    checksum_bytes(sql.as_bytes())
}

pub(crate) fn checksum_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The hook's scripts in run order, with checksums.
pub(crate) fn load_scripts(hook_name: &str, source: &Path) -> Result<Vec<Script>> {
    let not_found = || {
        AppError::Invalid(format!(
            "hook `{hook_name}`: {} does not exist",
            source.display()
        ))
    };
    let meta = std::fs::metadata(source).map_err(|_| not_found())?;
    let mut paths = Vec::new();
    if meta.is_dir() {
        for entry in std::fs::read_dir(source)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "sql") && path.is_file() {
                paths.push(path);
            }
        }
        paths.sort();
    } else {
        paths.push(source.to_path_buf());
    }
    if paths.is_empty() {
        return Err(AppError::Invalid(format!(
            "hook `{hook_name}`: no .sql files in {}",
            source.display()
        )));
    }
    paths
        .into_iter()
        .map(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !is_script_name(&name) {
                return Err(AppError::Invalid(format!(
                    "hook `{hook_name}`: script `{name}` must end in .sql and use only letters, digits, `.`, `_` and `-`"
                )));
            }
            let sql = std::fs::read_to_string(&p)?;
            Ok(Script { checksum: checksum(&sql), name, sql })
        })
        .collect()
}

/// Shell command, run in the service's container, that starts the engine's client.
/// Credentials and the default database come from the container's own environment.
pub(crate) fn client_command(engine: SqlEngine, database: Option<&str>, flags: &str) -> String {
    match engine {
        SqlEngine::Postgres => {
            let db = database
                .map(sh)
                .unwrap_or_else(|| r#""${POSTGRES_DB:-${POSTGRES_USER:-postgres}}""#.to_owned());
            format!(
                r#"exec psql -X -q -v ON_ERROR_STOP=1 {flags} -U "${{POSTGRES_USER:-postgres}}" -d {db}"#
            )
        }
        SqlEngine::Mysql => {
            let db = database
                .map(sh)
                .unwrap_or_else(|| r#""$MYSQL_DATABASE""#.to_owned());
            format!(r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -uroot {flags} {db}"#)
        }
    }
}

pub(crate) fn ensure_ledger_sql(engine: SqlEngine) -> &'static str {
    match engine {
        SqlEngine::Postgres => {
            "CREATE TABLE IF NOT EXISTS _ikit_hook_ledger (hook text NOT NULL, script text NOT NULL, \
             checksum text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (hook, script));\n"
        }
        SqlEngine::Mysql => {
            "CREATE TABLE IF NOT EXISTS _ikit_hook_ledger (hook VARCHAR(63) NOT NULL, script VARCHAR(255) NOT NULL, \
             checksum CHAR(64) NOT NULL, applied_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY (hook, script));\n"
        }
    }
}

fn select_ledger_sql(engine: SqlEngine, hook: &str) -> String {
    let concat = match engine {
        SqlEngine::Postgres => "script || '|' || checksum",
        SqlEngine::Mysql => "CONCAT(script, '|', checksum)",
    };
    format!("SELECT {concat} FROM _ikit_hook_ledger WHERE hook = '{hook}' ORDER BY script;\n")
}

/// The ledger upsert for `script`. Hook names, script names and checksums are validated to
/// a safe alphabet, so they can be inlined.
fn ledger_upsert_sql(engine: SqlEngine, hook: &str, script: &Script) -> String {
    let (name, sum) = (&script.name, &script.checksum);
    match engine {
        SqlEngine::Postgres => format!(
            "INSERT INTO _ikit_hook_ledger (hook, script, checksum) VALUES ('{hook}', '{name}', '{sum}') \
             ON CONFLICT (hook, script) DO UPDATE SET checksum = EXCLUDED.checksum, applied_at = now();\n"
        ),
        SqlEngine::Mysql => format!(
            "INSERT INTO _ikit_hook_ledger (hook, script, checksum) VALUES ('{hook}', '{name}', '{sum}') \
             ON DUPLICATE KEY UPDATE checksum = VALUES(checksum), applied_at = CURRENT_TIMESTAMP;\n"
        ),
    }
}

/// The script's SQL, ending in a terminated statement so the ledger insert is not glued on.
fn terminated(sql: &str) -> String {
    let body = sql.trim_end();
    let last_code = body
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or("");
    let terminator = if last_code.is_empty() || last_code.ends_with(';') {
        ""
    } else {
        "\n;"
    };
    format!("{body}{terminator}\n")
}

/// `sql` followed by its ledger row, as one transaction (for `exec`).
pub(crate) fn script_input(engine: SqlEngine, hook: &str, script: &Script) -> String {
    let body = terminated(&script.sql);
    let upsert = ledger_upsert_sql(engine, hook, script);
    match engine {
        // psql runs it with `-1` (single transaction).
        SqlEngine::Postgres => format!("{body}{upsert}"),
        SqlEngine::Mysql => format!("START TRANSACTION;\n{body}{upsert}COMMIT;\n"),
    }
}

pub(crate) fn parse_ledger(stdout: &str) -> BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|l| l.trim().split_once('|'))
        .map(|(s, c)| (s.to_owned(), c.to_owned()))
        .collect()
}

fn missing_table(stderr: &str) -> bool {
    stderr.contains("_ikit_hook_ledger")
        && (stderr.contains("does not exist") || stderr.contains("doesn't exist"))
}

/// The useful part of a client's error output. psql ends with the failing line and a `^`
/// marker; the `ERROR:` line (`ERROR 1366 ...` for mysql) is what explains it.
pub(crate) fn sql_error(out: &ExecOutput) -> String {
    let lines: Vec<&str> = out
        .stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    match lines.iter().position(|l| l.contains("ERROR")) {
        Some(i) => {
            let error = lines[i];
            let error = error
                .split_once("ERROR")
                .map_or(error, |(_, rest)| rest.trim_start_matches(':').trim());
            match lines.get(i + 1).filter(|l| l.starts_with("LINE ")) {
                Some(context) => format!("{error} ({context})"),
                None => error.to_owned(),
            }
        }
        None => out.failure_detail(),
    }
}

/// Where and as whom a `direct` hook connects.
#[derive(Clone)]
pub(crate) struct DirectTarget {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

/// A SQL hook bound to a workload's deployment, able to read the ledger and run scripts.
pub(crate) enum SqlConn<'a> {
    Exec {
        app: &'a App,
        place: &'a Place,
        workload: &'a str,
        action: &'a SqlAction,
    },
    Direct {
        engine: SqlEngine,
        target: DirectTarget,
    },
}

impl App {
    /// Resolves how a `direct` hook reaches its database: the service's published port for
    /// the engine, and the credentials from its env and the keychain.
    pub(crate) async fn direct_target(
        &self,
        workload: &str,
        action: &SqlAction,
    ) -> Result<DirectTarget> {
        let (instance, version) = self.service_show(workload, &action.service).await?;
        let default_port = match action.engine {
            SqlEngine::Postgres => 5432,
            SqlEngine::Mysql => 3306,
        };
        let container_port = version
            .spec
            .ports
            .first()
            .map_or(default_port, |p| p.container);
        let published = instance
            .publish
            .iter()
            .find(|p| p.container == container_port && !p.udp)
            .ok_or_else(|| {
                AppError::Invalid(format!(
                    "hook via direct needs `{}` to publish port {container_port}; run `ikit svc publish {} {container_port}:{container_port}` (or use --via exec)",
                    action.service, action.service
                ))
            })?;
        let wl = self.workload_get(workload).await?;
        let host = match (
            &self.target_show(&wl.target).await?.connection,
            &published.bind,
        ) {
            (_, Some(bind)) if bind != "0.0.0.0" && bind != "::" => bind.clone(),
            (Connection::Ssh { host, .. }, _) => host.clone(),
            _ => "127.0.0.1".to_owned(),
        };
        let env = self.service_env(workload, &instance.name).await?;
        let get = |k: &str| env.get(k).cloned();
        let (user, password, database) = match action.engine {
            SqlEngine::Postgres => {
                let user = get("POSTGRES_USER").unwrap_or_else(|| "postgres".into());
                let db = action
                    .database
                    .clone()
                    .or_else(|| get("POSTGRES_DB"))
                    .unwrap_or_else(|| user.clone());
                (user, get("POSTGRES_PASSWORD").unwrap_or_default(), db)
            }
            SqlEngine::Mysql => {
                let db = action
                    .database
                    .clone()
                    .or_else(|| get("MYSQL_DATABASE"))
                    .ok_or_else(|| {
                        AppError::Invalid(format!(
                            "`{}` sets no MYSQL_DATABASE; pass --database",
                            action.service
                        ))
                    })?;
                (
                    "root".to_owned(),
                    get("MYSQL_ROOT_PASSWORD").unwrap_or_default(),
                    db,
                )
            }
        };
        Ok(DirectTarget {
            host,
            port: published.host,
            user,
            password,
            database,
        })
    }
}

fn db_error(e: sqlx::Error) -> String {
    match e.as_database_error() {
        Some(db) => db.message().to_owned(),
        None => e.to_string(),
    }
}

/// Postgres 42P01 / MySQL 1146: the ledger table does not exist yet.
fn is_missing_table(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "42P01" || c == "42S02" || c == "1146")
}

impl SqlConn<'_> {
    fn engine(&self) -> SqlEngine {
        match self {
            SqlConn::Exec { action, .. } => action.engine,
            SqlConn::Direct { engine, .. } => *engine,
        }
    }

    async fn exec_client(&self, flags: &str, sql: &str, timeout: Duration) -> Result<ExecOutput> {
        let SqlConn::Exec {
            app,
            place,
            workload,
            action,
        } = self
        else {
            unreachable!("exec only")
        };
        let cmd = client_command(action.engine, action.database.as_deref(), flags);
        let args = [
            "exec",
            "-T",
            action.service.as_str(),
            "sh",
            "-c",
            cmd.as_str(),
        ];
        app.compose_with(place, workload, &args, Some(sql.as_bytes()), timeout)
            .await
    }

    /// The ledger for `hook`. With `create`, the table is created if missing; without it
    /// (planning), a missing table reads as empty. Errors are plain messages.
    pub(crate) async fn read_ledger(
        &self,
        hook: &str,
        create: bool,
    ) -> std::result::Result<BTreeMap<String, String>, String> {
        match self {
            SqlConn::Exec { .. } => {
                let flags = match self.engine() {
                    SqlEngine::Postgres => "-At",
                    SqlEngine::Mysql => "-N -B",
                };
                let mut sql = String::new();
                if create {
                    sql.push_str(ensure_ledger_sql(self.engine()));
                }
                sql.push_str(&select_ledger_sql(self.engine(), hook));
                let out = self
                    .exec_client(flags, &sql, LEDGER_TIMEOUT)
                    .await
                    .map_err(|e| e.to_string())?;
                if out.success() {
                    Ok(parse_ledger(&out.stdout))
                } else if !create && missing_table(&out.stderr) {
                    Ok(BTreeMap::new())
                } else {
                    Err(sql_error(&out))
                }
            }
            SqlConn::Direct { engine, target } => {
                let task = direct_read_ledger(*engine, target.clone(), hook.to_owned(), create);
                tokio::spawn(task).await.map_err(|e| e.to_string())?
            }
        }
    }

    /// Runs `script` and records it in the ledger, in one transaction. Errors are plain messages.
    pub(crate) async fn run_script(
        &self,
        hook: &str,
        script: &Script,
    ) -> std::result::Result<(), String> {
        match self {
            SqlConn::Exec { .. } => {
                let flags = match self.engine() {
                    SqlEngine::Postgres => "-1",
                    SqlEngine::Mysql => "",
                };
                let input = script_input(self.engine(), hook, script);
                let out = self
                    .exec_client(flags, &input, SCRIPT_TIMEOUT)
                    .await
                    .map_err(|e| e.to_string())?;
                if out.success() {
                    Ok(())
                } else {
                    Err(sql_error(&out))
                }
            }
            SqlConn::Direct { engine, target } => {
                let upsert = ledger_upsert_sql(*engine, hook, script);
                let task = direct_run(*engine, target.clone(), script.sql.clone(), upsert);
                tokio::spawn(task).await.map_err(|e| e.to_string())?
            }
        }
    }
}

// Direct database work runs in its own task over owned data: sqlx's executor lifetimes do not
// survive being nested inside other `Send` futures.

async fn direct_read_ledger(
    engine: SqlEngine,
    target: DirectTarget,
    hook: String,
    create: bool,
) -> std::result::Result<BTreeMap<String, String>, String> {
    use sqlx::{Executor, Row};
    let query = format!("SELECT script, checksum FROM _ikit_hook_ledger WHERE hook = '{hook}'");
    let rows: std::result::Result<Vec<(String, String)>, sqlx::Error> = match engine {
        SqlEngine::Postgres => {
            let mut conn = pg_connect(&target).await?;
            if create {
                conn.execute(ensure_ledger_sql(engine))
                    .await
                    .map_err(db_error)?;
            }
            conn.fetch_all(query.as_str()).await.and_then(|rows| {
                rows.iter()
                    .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
                    .collect()
            })
        }
        SqlEngine::Mysql => {
            let mut conn = mysql_connect(&target).await?;
            if create {
                conn.execute(ensure_ledger_sql(engine))
                    .await
                    .map_err(db_error)?;
            }
            conn.fetch_all(query.as_str()).await.and_then(|rows| {
                rows.iter()
                    .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
                    .collect()
            })
        }
    };
    match rows {
        Ok(rows) => Ok(rows.into_iter().collect()),
        Err(e) if !create && is_missing_table(&e) => Ok(BTreeMap::new()),
        Err(e) => Err(db_error(e)),
    }
}

/// Runs `sql` then `upsert` in one transaction. `&str` queries go over the simple protocol,
/// so scripts may hold several statements.
async fn direct_run(
    engine: SqlEngine,
    target: DirectTarget,
    sql: String,
    upsert: String,
) -> std::result::Result<(), String> {
    use sqlx::Executor;
    match engine {
        SqlEngine::Postgres => {
            let mut conn = pg_connect(&target).await?;
            let mut tx = conn.begin().await.map_err(db_error)?;
            tx.execute(sql.as_str()).await.map_err(db_error)?;
            tx.execute(upsert.as_str()).await.map_err(db_error)?;
            tx.commit().await.map_err(db_error)
        }
        SqlEngine::Mysql => {
            let mut conn = mysql_connect(&target).await?;
            let mut tx = conn.begin().await.map_err(db_error)?;
            tx.execute(sql.as_str()).await.map_err(db_error)?;
            tx.execute(upsert.as_str()).await.map_err(db_error)?;
            tx.commit().await.map_err(db_error)
        }
    }
}

async fn pg_connect(t: &DirectTarget) -> std::result::Result<sqlx::PgConnection, String> {
    let opts = sqlx::postgres::PgConnectOptions::new()
        .host(&t.host)
        .port(t.port)
        .username(&t.user)
        .password(&t.password)
        .database(&t.database);
    sqlx::PgConnection::connect_with(&opts).await.map_err(|e| {
        format!(
            "connecting to {}:{} as {}: {}",
            t.host,
            t.port,
            t.user,
            db_error(e)
        )
    })
}

async fn mysql_connect(t: &DirectTarget) -> std::result::Result<sqlx::MySqlConnection, String> {
    let opts = sqlx::mysql::MySqlConnectOptions::new()
        .host(&t.host)
        .port(t.port)
        .username(&t.user)
        .password(&t.password)
        .database(&t.database);
    sqlx::MySqlConnection::connect_with(&opts)
        .await
        .map_err(|e| {
            format!(
                "connecting to {}:{} as {}: {}",
                t.host,
                t.port,
                t.user,
                db_error(e)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(sql: &str) -> Script {
        Script {
            name: "001_init.sql".into(),
            checksum: checksum(sql),
            sql: sql.into(),
        }
    }

    #[test]
    fn script_input_appends_ledger_row_safely() {
        let pg = script_input(
            SqlEngine::Postgres,
            "schema",
            &script("CREATE TABLE t (id int)\n-- trailing comment\n"),
        );
        assert!(
            pg.contains(
                "CREATE TABLE t (id int)\n-- trailing comment\n;\nINSERT INTO _ikit_hook_ledger"
            ),
            "{pg}"
        );
        assert!(pg.contains("VALUES ('schema', '001_init.sql', '"));

        let terminated = script_input(
            SqlEngine::Mysql,
            "schema",
            &script("CREATE TABLE t (id int);"),
        );
        assert!(
            terminated.starts_with("START TRANSACTION;\nCREATE TABLE t (id int);\nINSERT"),
            "{terminated}"
        );
        assert!(terminated.ends_with("COMMIT;\n"));
    }

    #[test]
    fn client_commands_use_container_env() {
        let pg = client_command(SqlEngine::Postgres, None, "-1");
        assert!(
            pg.contains(
                r#"-U "${POSTGRES_USER:-postgres}" -d "${POSTGRES_DB:-${POSTGRES_USER:-postgres}}""#
            ),
            "{pg}"
        );
        assert!(
            client_command(SqlEngine::Postgres, Some("orders"), "-At").ends_with("-d 'orders'")
        );
        let my = client_command(SqlEngine::Mysql, None, "-N -B");
        assert!(
            my.starts_with(r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" exec mysql -uroot -N -B"#),
            "{my}"
        );
    }

    #[test]
    fn loads_folders_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("002_b.sql"), "b").unwrap();
        std::fs::write(dir.path().join("001_a.sql"), "a").unwrap();
        std::fs::write(dir.path().join("notes.md"), "ignored").unwrap();
        let names: Vec<String> = load_scripts("h", dir.path())
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["001_a.sql", "002_b.sql"]);

        std::fs::write(dir.path().join("bad name.sql"), "x").unwrap();
        assert!(load_scripts("h", dir.path()).is_err());
        assert!(load_scripts("h", &dir.path().join("missing")).is_err());
    }

    #[test]
    fn extracts_the_error_line() {
        let out = ExecOutput {
            code: Some(3),
            stderr: "psql:<stdin>:2: ERROR:  invalid input syntax for type integer: \"x\"\nLINE 1: INSERT INTO refunds VALUES ('x');\n                                    ^\n".into(),
            ..Default::default()
        };
        assert_eq!(
            sql_error(&out),
            r#"invalid input syntax for type integer: "x" (LINE 1: INSERT INTO refunds VALUES ('x');)"#
        );
        let mysql = ExecOutput {
            code: Some(1),
            stderr: "ERROR 1366 (HY000) at line 3: Incorrect integer value: 'x'".into(),
            ..Default::default()
        };
        assert_eq!(
            sql_error(&mysql),
            "1366 (HY000) at line 3: Incorrect integer value: 'x'"
        );
    }

    #[test]
    fn parses_ledger_output() {
        let l = parse_ledger("001_init.sql|abc\n R__views.sql|def \n\n");
        assert_eq!(l.len(), 2);
        assert_eq!(l["R__views.sql"], "def");
        assert!(missing_table(
            r#"ERROR:  relation "_ikit_hook_ledger" does not exist"#
        ));
    }
}
