//! Scripts: an org-level library of shell (or Python) scripts that ikit runs against a
//! workload. A script declares the env vars it needs and where each value comes from (a
//! service's env, the target's ssh destination, a published port, ...); ikit resolves them
//! from its own state at run time, so scripts carry no hostnames, container names or
//! passwords.
//!
//! A script runs on the workload's target host (in its compose project folder), inside one
//! of its service containers, or on the machine running ikit. Runs that span two workloads
//! (e.g. copying a production dump into dev) name a `from` workload as well as the primary
//! one; sources prefixed `from.` read the former.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::registry::Category;
use crate::workload::RunResult;

string_enum! {
    "interpreter",
    pub enum Interpreter {
        Sh => "sh",
        Bash => "bash",
        Python3 => "python3",
        /// SQL run by a database service's own client (`psql` or `mysql`) in its container.
        Sql => "sql",
    }
}

impl Interpreter {
    /// File extension the script is written with on the machine that runs it.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Sh | Self::Bash => "sh",
            Self::Python3 => "py",
            Self::Sql => "sql",
        }
    }

    /// Guesses from the file name (`.sql`, `.py`), else from the `#!` line.
    pub fn guess(file_name: &str, body: &str) -> Self {
        match file_name.rsplit_once('.').map(|(_, ext)| ext) {
            Some("sql") => Self::Sql,
            Some("py") => Self::Python3,
            _ => Self::from_shebang(body),
        }
    }

    /// Guesses from a `#!` line: python, bash, or plain `sh`.
    pub fn from_shebang(body: &str) -> Self {
        match body.lines().next().and_then(|l| l.strip_prefix("#!")) {
            Some(l) if l.contains("python") => Self::Python3,
            Some(l) if l.contains("bash") => Self::Bash,
            _ => Self::Sh,
        }
    }
}

/// Where a script runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum RunsOn {
    /// On the workload's target host, in its compose project folder.
    Target,
    /// On the machine running ikit.
    Local,
    /// Inside this service's running container.
    Service(String),
}

impl fmt::Display for RunsOn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Target => f.write_str("target"),
            Self::Local => f.write_str("local"),
            Self::Service(s) => write!(f, "service:{s}"),
        }
    }
}

impl FromStr for RunsOn {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "target" => Ok(Self::Target),
            "local" => Ok(Self::Local),
            _ => match s.strip_prefix("service:") {
                Some(name) if crate::is_slug(name) => Ok(Self::Service(name.to_owned())),
                _ => Err(format!(
                    "invalid runs_on `{s}` (expected target, local or service:<name>)"
                )),
            },
        }
    }
}

impl TryFrom<String> for RunsOn {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<RunsOn> for String {
    fn from(r: RunsOn) -> Self {
        r.to_string()
    }
}

/// Which workload a source reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The workload the script runs against (`--to`, or the selected workload).
    Primary,
    /// The `--from` workload.
    From,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetField {
    Name,
    /// The ssh host (`127.0.0.1` for a local target).
    Host,
    User,
    /// The ssh port (22 unless set).
    Port,
    /// `[user@]host`, for `ssh` and `scp`.
    Ssh,
    /// The options ikit passes to ssh before the destination (port, identity file, batch
    /// mode), space-separated.
    SshArgs,
    /// The workload's compose project folder on the target.
    Dir,
}

impl TargetField {
    const ALL: [(Self, &'static str); 7] = [
        (Self::Name, "name"),
        (Self::Host, "host"),
        (Self::User, "user"),
        (Self::Port, "port"),
        (Self::Ssh, "ssh"),
        (Self::SshArgs, "ssh_args"),
        (Self::Dir, "dir"),
    ];

    fn as_str(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(f, _)| *f == self)
            .expect("listed")
            .1
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ServiceField {
    /// The container's name, `<workload>-<service>-1`.
    Container,
    /// The service's DNS name inside the workload's networks.
    Host,
    Image,
    Version,
    /// A value of the service's env: registry default, instance value or keychain secret.
    Env(String),
    /// The host port a container port is published on.
    Port(u16),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Field {
    Org,
    Workload,
    Env,
    Phase,
    RunId,
    Target(TargetField),
    Service { name: String, field: ServiceField },
}

/// Where a script param's value comes from, e.g. `service.postgres.env.POSTGRES_PASSWORD`,
/// `target.ssh` or `from.target.dir`. A `to.` prefix is accepted and means the primary
/// workload, like no prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SourcePath {
    pub side: Side,
    pub field: Field,
}

pub const SOURCE_HELP: &str = "org, workload, env, phase, run_id, target.{name,host,user,port,ssh,ssh_args,dir}, service.<name>.{container,host,image,version}, service.<name>.env.<VAR> or service.<name>.port.<container port>, optionally prefixed with from. or to.";

impl fmt::Display for SourcePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.side == Side::From {
            f.write_str("from.")?;
        }
        match &self.field {
            Field::Org => f.write_str("org"),
            Field::Workload => f.write_str("workload"),
            Field::Env => f.write_str("env"),
            Field::Phase => f.write_str("phase"),
            Field::RunId => f.write_str("run_id"),
            Field::Target(t) => write!(f, "target.{}", t.as_str()),
            Field::Service { name, field } => {
                write!(f, "service.{name}.")?;
                match field {
                    ServiceField::Container => f.write_str("container"),
                    ServiceField::Host => f.write_str("host"),
                    ServiceField::Image => f.write_str("image"),
                    ServiceField::Version => f.write_str("version"),
                    ServiceField::Env(v) => write!(f, "env.{v}"),
                    ServiceField::Port(p) => write!(f, "port.{p}"),
                }
            }
        }
    }
}

impl FromStr for SourcePath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("invalid source `{s}` (expected {SOURCE_HELP})");
        let (side, rest) = if let Some(r) = s.strip_prefix("from.") {
            (Side::From, r)
        } else if let Some(r) = s.strip_prefix("to.") {
            (Side::Primary, r)
        } else {
            (Side::Primary, s)
        };
        let parts: Vec<&str> = rest.split('.').collect();
        let field = match parts.as_slice() {
            ["org"] => Field::Org,
            ["workload"] => Field::Workload,
            ["env"] => Field::Env,
            ["phase"] => Field::Phase,
            ["run_id"] => Field::RunId,
            ["target", f] => Field::Target(
                TargetField::ALL
                    .iter()
                    .find(|(_, n)| n == f)
                    .map(|(t, _)| *t)
                    .ok_or_else(bad)?,
            ),
            ["service", name, rest @ ..] if crate::is_slug(name) => {
                let field = match rest {
                    ["container"] => ServiceField::Container,
                    ["host"] => ServiceField::Host,
                    ["image"] => ServiceField::Image,
                    ["version"] => ServiceField::Version,
                    ["env", var] if is_env_name(var) => ServiceField::Env((*var).to_owned()),
                    ["port", p] => ServiceField::Port(p.parse().map_err(|_| bad())?),
                    _ => return Err(bad()),
                };
                Field::Service {
                    name: (*name).to_owned(),
                    field,
                }
            }
            _ => return Err(bad()),
        };
        if side == Side::From && matches!(field, Field::Org | Field::Phase | Field::RunId) {
            return Err(format!(
                "invalid source `{s}`: org, phase and run_id have no from. form"
            ));
        }
        Ok(Self { side, field })
    }
}

impl TryFrom<String> for SourcePath {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<SourcePath> for String {
    fn from(s: SourcePath) -> Self {
        s.to_string()
    }
}

/// One env var a script runs with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptParam {
    /// The env var name the script reads.
    pub name: String,
    /// Resolved from ikit's state at run time. Exactly one of `from` and `value` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<SourcePath>,
    /// A fixed value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Used when `from` has no value (an unset env var, an unpublished port).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Whether a missing value (and no default) fails the run; otherwise the var is unset.
    #[serde(default = "yes")]
    pub required: bool,
    /// Masked in previews and history. Values from a service's secret env are always masked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
}

fn yes() -> bool {
    true
}

impl ScriptParam {
    /// Where the value comes from, for display: the source path or `value`.
    pub fn describe(&self) -> String {
        match (&self.from, &self.value) {
            (Some(from), _) => from.to_string(),
            (None, Some(_)) => "value".to_owned(),
            (None, None) => String::new(),
        }
    }
}

/// Rows built from the services' env and handed to the script as a CSV file (e.g. one row
/// per database to create).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CsvInput {
    /// The first line of the file.
    pub header: String,
    /// Env var names, one per column. A service gets a row when it has a value for each.
    pub columns: Vec<String>,
    /// Only services in this group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Category>,
    /// Also pass the file's path as the script's last argument (it is always in
    /// `$IKIT_INPUT`).
    #[serde(default = "yes")]
    pub arg: bool,
}

pub const DEFAULT_TIMEOUT_SECS: u64 = 10 * 60;

/// What a script needs, declared in its `# ikit:` header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ScriptParam>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csv: Option<CsvInput>,
    /// Arguments always passed to the script, before any given at run time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// SQL scripts: the database to run in (default: the one the service's env configures).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// SQL scripts: run the whole script in one transaction.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub transaction: bool,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

impl Default for ScriptSpec {
    fn default() -> Self {
        Self {
            params: vec![],
            csv: None,
            args: vec![],
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            database: None,
            transaction: false,
        }
    }
}

impl ScriptSpec {
    /// Whether any param reads the `from` workload.
    pub fn uses_from(&self) -> bool {
        self.params
            .iter()
            .any(|p| p.from.as_ref().is_some_and(|f| f.side == Side::From))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Script {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub interpreter: Interpreter,
    pub runs_on: RunsOn,
    /// The file as imported, header included.
    pub body: String,
    /// sha256 of `body`.
    pub checksum: String,
    pub spec: ScriptSpec,
    /// The path it was imported from, for reference.
    pub source: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Script {
    /// The file name it is written to, e.g. `create-db.sh`.
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.name, self.interpreter.extension())
    }
}

/// One execution of a script, on demand or by a hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptRunRecord {
    pub script: String,
    pub workload: String,
    pub from_workload: Option<String>,
    /// The hook that ran it, if a hook did.
    pub hook: Option<String>,
    pub run_id: Option<i64>,
    pub checksum: String,
    /// Resolved params; secret values are `<secret>`.
    pub params: BTreeMap<String, String>,
    pub result: RunResult,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// The end of the script's output.
    pub output_tail: Option<String>,
    pub ran_at: DateTime<Utc>,
}

/// What a masked secret looks like in previews and history.
pub const MASK: &str = "<secret>";

/// Names safe to `export` in a POSIX shell.
pub fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_round_trip() {
        for s in [
            "org",
            "workload",
            "env",
            "phase",
            "run_id",
            "target.ssh",
            "target.ssh_args",
            "from.target.dir",
            "service.postgres.container",
            "service.postgres.env.POSTGRES_PASSWORD",
            "from.service.pg-main.port.5432",
            "service.core.version",
        ] {
            let p: SourcePath = s.parse().unwrap();
            assert_eq!(p.to_string(), s);
        }
        let to: SourcePath = "to.target.ssh".parse().unwrap();
        assert_eq!(to, "target.ssh".parse().unwrap());
        assert_eq!(
            "from.service.pg.env.X".parse::<SourcePath>().unwrap().field,
            Field::Service {
                name: "pg".into(),
                field: ServiceField::Env("X".into())
            }
        );
    }

    #[test]
    fn rejects_bad_sources() {
        for s in [
            "",
            "target",
            "target.password",
            "service.Postgres.host",
            "service.pg",
            "service.pg.env",
            "service.pg.env.BAD-NAME",
            "service.pg.port.http",
            "service.pg.port.70000",
            "from.org",
            "from.phase",
            "secrets.x",
        ] {
            assert!(s.parse::<SourcePath>().is_err(), "{s}");
        }
    }

    #[test]
    fn runs_on_round_trips() {
        for s in ["target", "local", "service:postgres"] {
            assert_eq!(s.parse::<RunsOn>().unwrap().to_string(), s);
        }
        for bad in ["host", "service:", "service:Bad", "service"] {
            assert!(bad.parse::<RunsOn>().is_err(), "{bad}");
        }
        let json = serde_json::to_string(&RunsOn::Service("pg".into())).unwrap();
        assert_eq!(json, r#""service:pg""#);
    }

    #[test]
    fn guesses_interpreters() {
        assert_eq!(
            Interpreter::from_shebang("#!/usr/bin/env bash\necho"),
            Interpreter::Bash
        );
        assert_eq!(
            Interpreter::from_shebang("#!/usr/bin/python3\n"),
            Interpreter::Python3
        );
        assert_eq!(Interpreter::from_shebang("#!/bin/sh\n"), Interpreter::Sh);
        assert_eq!(Interpreter::from_shebang("echo hi\n"), Interpreter::Sh);
        assert_eq!(
            Interpreter::guess("grants.sql", "#!/bin/sh"),
            Interpreter::Sql
        );
        assert_eq!(Interpreter::guess("x.py", ""), Interpreter::Python3);
        assert_eq!(Interpreter::guess("x", "#!/bin/bash\n"), Interpreter::Bash);
    }

    #[test]
    fn specs_serialize_compactly() {
        let spec = ScriptSpec {
            params: vec![ScriptParam {
                name: "PGPASS".into(),
                from: Some("service.pg.env.POSTGRES_PASSWORD".parse().unwrap()),
                value: None,
                default: None,
                required: true,
                secret: false,
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(
            json,
            r#"{"params":[{"name":"PGPASS","from":"service.pg.env.POSTGRES_PASSWORD","required":true}],"timeout_secs":600}"#
        );
        assert_eq!(serde_json::from_str::<ScriptSpec>(&json).unwrap(), spec);
        assert_eq!(
            serde_json::from_str::<ScriptSpec>("{}").unwrap(),
            ScriptSpec::default()
        );
        assert!(!spec.uses_from());
    }
}
