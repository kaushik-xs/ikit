//! Hooks: actions run around a workload's deploys. SQL hooks run scripts in a database
//! service after apply; shell hooks run a command in a service or on the target; HTTP hooks
//! call a URL (e.g. a chat webhook); MCP hooks call a tool on an MCP server that ikit starts
//! over stdio (e.g. to post to a chat, open a ticket or warm a cache).
//!
//! Each script is recorded in a ledger inside the target database with its checksum, so a
//! script runs once. Scripts named `R__*.sql` are repeatable: they run again whenever their
//! content changes. Changing an already-applied script is refused unless explicitly allowed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::workload::RunResult;

string_enum! {
    "hook kind",
    pub enum HookKind {
        Sql => "sql",
        Shell => "shell",
        Http => "http",
        Mcp => "mcp",
    }
}

string_enum! {
    "hook phase",
    pub enum HookPhase {
        /// Before the compose files are written and services started.
        PreApply => "pre-apply",
        /// After services are up and healthy.
        PostApply => "post-apply",
        /// Before containers are removed.
        PreDestroy => "pre-destroy",
    }
}

string_enum! {
    "sql engine",
    pub enum SqlEngine {
        Postgres => "postgres",
        Mysql => "mysql",
    }
}

impl SqlEngine {
    /// The engine for a registry service name, if it is a known database image.
    pub fn detect(registry_service: &str) -> Option<Self> {
        match registry_service {
            "postgres" | "postgresql" | "timescaledb" | "postgis" => Some(Self::Postgres),
            "mysql" | "mariadb" => Some(Self::Mysql),
            _ => None,
        }
    }
}

string_enum! {
    "sql connection",
    /// How SQL reaches the database.
    pub enum SqlVia {
        /// `docker compose exec` into the service's container and use its client.
        Exec => "exec",
        /// Connect to the service's published port from the machine running ikit.
        Direct => "direct",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlAction {
    /// The service instance the scripts run in.
    pub service: String,
    pub engine: SqlEngine,
    /// Database to connect to; defaults to the one the service's env configures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// A `.sql` file, or a folder whose `*.sql` files run in name order.
    pub source: PathBuf,
    #[serde(default = "default_via")]
    pub via: SqlVia,
}

fn default_via() -> SqlVia {
    SqlVia::Exec
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAction {
    /// Run with `sh -c`. `$IKIT_ORG`, `$IKIT_WORKLOAD`, `$IKIT_ENV`, `$IKIT_PHASE` and
    /// `$IKIT_RUN_ID` are set.
    pub command: String,
    /// Run inside this service's container; `None` runs on the target host, in the
    /// workload's compose project folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpAction {
    pub method: String,
    /// Scheme and host only; the full URL (often containing a token) is in the keychain.
    pub url_display: String,
    /// Request body; `{{org}}`, `{{workload}}`, `{{env}}`, `{{target}}`, `{{phase}}` and
    /// `{{run_id}}` are replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAction {
    /// The MCP server to start, as program and arguments, run on the machine running ikit
    /// (e.g. `["npx", "-y", "@acme/mcp-server"]`). `IKIT_ORG`, `IKIT_WORKLOAD`, `IKIT_ENV`,
    /// `IKIT_PHASE` and `IKIT_RUN_ID` are set in its environment.
    pub command: Vec<String>,
    pub tool: String,
    /// Tool arguments, a JSON object; `{{org}}`, `{{workload}}` and friends are replaced in
    /// its string values.
    #[serde(default = "empty_object")]
    pub arguments: serde_json::Value,
    /// Env vars for the server whose values are in the keychain (e.g. an API token).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_env: Vec<String>,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

impl McpAction {
    /// `arguments` with placeholders replaced in every string value.
    pub fn expanded_arguments(&self, vars: &BTreeMap<&str, String>) -> serde_json::Value {
        fn walk(v: &serde_json::Value, vars: &BTreeMap<&str, String>) -> serde_json::Value {
            use serde_json::Value;
            match v {
                Value::String(s) => Value::String(expand(s, vars)),
                Value::Array(a) => Value::Array(a.iter().map(|v| walk(v, vars)).collect()),
                Value::Object(o) => {
                    Value::Object(o.iter().map(|(k, v)| (k.clone(), walk(v, vars))).collect())
                }
                other => other.clone(),
            }
        }
        walk(&self.arguments, vars)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum HookAction {
    Sql(SqlAction),
    Shell(ShellAction),
    Http(HttpAction),
    Mcp(McpAction),
}

impl HookAction {
    pub fn kind(&self) -> HookKind {
        match self {
            Self::Sql(_) => HookKind::Sql,
            Self::Shell(_) => HookKind::Shell,
            Self::Http(_) => HookKind::Http,
            Self::Mcp(_) => HookKind::Mcp,
        }
    }

    /// The service the hook runs in, if any.
    pub fn service(&self) -> Option<&str> {
        match self {
            Self::Sql(a) => Some(&a.service),
            Self::Shell(a) => a.service.as_deref(),
            Self::Http(_) | Self::Mcp(_) => None,
        }
    }

    /// One-line description, e.g. `sql postgres <- db/migrations`.
    pub fn describe(&self) -> String {
        match self {
            Self::Sql(a) => format!(
                "sql in {} via {} <- {}",
                a.service,
                a.via,
                a.source.display()
            ),
            Self::Shell(a) => match &a.service {
                Some(s) => format!("shell in {s}: {}", a.command),
                None => format!("shell on target: {}", a.command),
            },
            Self::Http(a) => format!("http {} {}", a.method, a.url_display),
            Self::Mcp(a) => format!("mcp {} via `{}`", a.tool, a.command.join(" ")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hook {
    pub id: i64,
    pub name: String,
    pub phase: HookPhase,
    /// Whether a failure fails the apply or destroy; otherwise it is reported as a warning.
    pub fail_on_error: bool,
    #[serde(flatten)]
    pub action: HookAction,
    /// Hooks run in ascending order.
    pub ord: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Hook {
    pub fn sql(&self) -> Option<&SqlAction> {
        match &self.action {
            HookAction::Sql(a) => Some(a),
            _ => None,
        }
    }
}

/// Replaces `{{key}}` placeholders with `vars`.
pub fn expand(template: &str, vars: &BTreeMap<&str, String>) -> String {
    vars.iter().fold(template.to_owned(), |acc, (k, v)| {
        acc.replace(&format!("{{{{{k}}}}}"), v)
    })
}

/// One executed script, mirrored from the target's ledger into the state database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookRunRecord {
    pub hook: String,
    pub script: String,
    pub checksum: String,
    pub result: RunResult,
    pub error: Option<String>,
    pub ran_at: DateTime<Utc>,
}

/// Script file names go into SQL and shell commands, so they are kept to a safe alphabet.
pub fn is_script_name(name: &str) -> bool {
    name.len() <= 200
        && name.ends_with(".sql")
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub fn is_repeatable(script: &str) -> bool {
    script.starts_with("R__")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptStatus {
    /// Never applied: will run.
    New,
    /// A repeatable script whose content changed: will run again.
    Rerun,
    /// Applied with the same checksum: skipped.
    Applied,
    /// An applied, non-repeatable script whose content changed: refused unless allowed.
    Changed,
}

impl ScriptStatus {
    pub fn will_run(self, allow_changed: bool) -> bool {
        match self {
            Self::New | Self::Rerun => true,
            Self::Changed => allow_changed,
            Self::Applied => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptState {
    pub script: String,
    pub checksum: String,
    pub status: ScriptStatus,
}

/// Compares local scripts `(name, checksum)` with the target's ledger (`name → checksum`).
pub fn classify(
    scripts: &[(String, String)],
    ledger: &BTreeMap<String, String>,
) -> Vec<ScriptState> {
    scripts
        .iter()
        .map(|(script, checksum)| {
            let status = match ledger.get(script) {
                None => ScriptStatus::New,
                Some(applied) if applied == checksum => ScriptStatus::Applied,
                Some(_) if is_repeatable(script) => ScriptStatus::Rerun,
                Some(_) => ScriptStatus::Changed,
            };
            ScriptState {
                script: script.clone(),
                checksum: checksum.clone(),
                status,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str, sum: &str) -> (String, String) {
        (name.to_owned(), sum.to_owned())
    }

    #[test]
    fn classifies_against_the_ledger() {
        let scripts = [
            s("001_init.sql", "a"),
            s("002_users.sql", "b2"),
            s("R__views.sql", "v2"),
            s("003_new.sql", "c"),
        ];
        let ledger = BTreeMap::from([
            ("001_init.sql".to_owned(), "a".to_owned()),
            ("002_users.sql".to_owned(), "b1".to_owned()),
            ("R__views.sql".to_owned(), "v1".to_owned()),
        ]);
        let states: Vec<ScriptStatus> = classify(&scripts, &ledger)
            .into_iter()
            .map(|s| s.status)
            .collect();
        use ScriptStatus::*;
        assert_eq!(states, [Applied, Changed, Rerun, New]);
        assert!(!Changed.will_run(false) && Changed.will_run(true));
        assert!(!Applied.will_run(true));
    }

    #[test]
    fn validates_script_names() {
        for ok in ["001_init.sql", "R__views.sql", "V2-add-users.sql"] {
            assert!(is_script_name(ok), "{ok}");
        }
        for bad in [
            "init.txt",
            ".hidden.sql",
            "drop table.sql",
            "x';--.sql",
            "../a.sql",
        ] {
            assert!(!is_script_name(bad), "{bad}");
        }
    }

    #[test]
    fn expands_placeholders() {
        let vars = BTreeMap::from([
            ("workload", "wl-acme-dev-001".to_owned()),
            ("phase", "post-apply".to_owned()),
        ]);
        assert_eq!(
            expand(r#"{"text":"{{workload}} {{phase}} {{unknown}}"}"#, &vars),
            r#"{"text":"wl-acme-dev-001 post-apply {{unknown}}"}"#
        );
    }

    #[test]
    fn actions_serialize_with_kind_tag() {
        let a = HookAction::Shell(ShellAction {
            command: "echo hi".into(),
            service: None,
        });
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"kind":"shell","command":"echo hi"}"#
        );
        let back: HookAction = serde_json::from_str(
            r#"{"kind":"sql","service":"pg","engine":"postgres","source":"/x"}"#,
        )
        .unwrap();
        assert_eq!(back.kind(), HookKind::Sql);
        assert_eq!(back.service(), Some("pg"));
    }

    #[test]
    fn detects_engines() {
        assert_eq!(SqlEngine::detect("postgres"), Some(SqlEngine::Postgres));
        assert_eq!(SqlEngine::detect("mariadb"), Some(SqlEngine::Mysql));
        assert_eq!(SqlEngine::detect("redis"), None);
    }
}
