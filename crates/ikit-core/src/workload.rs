//! Workloads (`{prefix}-{env}-{nnn}`) and the service instances inside their groups.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use semver::Version;
use serde::Serialize;

use crate::registry::Category;
use crate::target::TargetKind;
use crate::version::VersionConstraint;

string_enum! {
    "workload status",
    pub enum WorkloadStatus {
        /// Defined but not running (never applied, or destroyed).
        Draft => "draft",
        /// The last apply succeeded.
        Applied => "applied",
        /// The last apply or destroy failed; the target may be partly updated.
        Failed => "failed",
    }
}

/// `{prefix}-{env}-{nnn}`, e.g. `wl-acme-dev-001`. The prefix may contain `-`; the env
/// may not, so names split unambiguously from the right. Numbers go past 999 if needed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkloadName {
    pub prefix: String,
    pub env: String,
    pub seq: u32,
}

impl WorkloadName {
    pub fn new(prefix: impl Into<String>, env: impl Into<String>, seq: u32) -> Self {
        Self {
            prefix: prefix.into(),
            env: env.into(),
            seq,
        }
    }
}

impl fmt::Display for WorkloadName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}-{:03}", self.prefix, self.env, self.seq)
    }
}

impl FromStr for WorkloadName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            format!(
                "invalid workload name `{s}` (expected <prefix>-<env>-<nnn>, e.g. wl-acme-dev-001)"
            )
        };
        let mut parts = s.rsplitn(3, '-');
        let (Some(seq), Some(env), Some(prefix)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        if !crate::is_slug(prefix)
            || !is_env(env)
            || seq.len() < 3
            || !seq.chars().all(|c| c.is_ascii_digit())
        {
            return Err(invalid());
        }
        let seq = seq.parse().map_err(|_| invalid())?;
        if seq == 0 {
            return Err(invalid());
        }
        Ok(Self::new(prefix, env, seq))
    }
}

/// Environment names are short lowercase words (no `-`, so workload names stay unambiguous).
pub fn is_env(env: &str) -> bool {
    (1..=16).contains(&env.len())
        && env.starts_with(|c: char| c.is_ascii_lowercase())
        && env
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Workload {
    pub id: i64,
    pub name: String,
    pub env: String,
    pub seq: u32,
    pub target: String,
    pub target_kind: TargetKind,
    pub status: WorkloadStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A registry service added to a workload's service group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceInstance {
    pub id: i64,
    pub name: String,
    pub group: Category,
    pub registry_service: String,
    pub constraint: VersionConstraint,
    pub version_id: i64,
    pub version: Version,
    pub image: String,
    /// Pinned image digest of the resolved version (`sha256:…`), if any.
    pub digest: Option<String>,
    pub deprecated: bool,
    /// Non-secret env values set for this instance (registry defaults are not copied in).
    pub env: BTreeMap<String, String>,
    /// Env vars whose values are in the keychain.
    pub secret_env: Vec<String>,
    /// Container ports published on the target's host.
    pub publish: Vec<PublishedPort>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

string_enum! {
    "run action",
    pub enum RunAction {
        Apply => "apply",
        Destroy => "destroy",
        /// `ikit hook run` outside an apply.
        Hook => "hook",
        Backup => "backup",
        Restore => "restore",
    }
}

string_enum! {
    "run result",
    pub enum RunResult {
        Running => "running",
        Ok => "ok",
        Failed => "failed",
    }
}

/// One `apply` or `destroy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Run {
    pub id: i64,
    pub action: RunAction,
    /// Who ran it, e.g. `cli:kaushik@laptop`.
    pub actor: String,
    pub result: RunResult,
    pub error: Option<String>,
    /// The plan as JSON.
    pub plan: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// `[BIND:]HOST:CONTAINER[/udp]`, e.g. `8080:80` or `127.0.0.1:5432:5432`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct PublishedPort {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    pub host: u16,
    pub container: u16,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub udp: bool,
}

impl fmt::Display for PublishedPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(bind) = &self.bind {
            write!(f, "{bind}:")?;
        }
        write!(f, "{}:{}", self.host, self.container)?;
        if self.udp {
            f.write_str("/udp")?;
        }
        Ok(())
    }
}

impl FromStr for PublishedPort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            format!(
                "invalid port mapping `{s}` (expected [BIND:]HOST:CONTAINER[/udp], e.g. 8080:80)"
            )
        };
        let (rest, udp) = match s.strip_suffix("/udp") {
            Some(r) => (r, true),
            None => (s.strip_suffix("/tcp").unwrap_or(s), false),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        let (bind, host, container) = match parts.as_slice() {
            [h, c] => (None, *h, *c),
            [b, h, c] if !b.is_empty() => (Some((*b).to_owned()), *h, *c),
            _ => return Err(invalid()),
        };
        let port = |p: &str| p.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(invalid);
        Ok(Self {
            bind,
            host: port(host)?,
            container: port(container)?,
            udp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_ports_round_trip() {
        for s in ["8080:80", "127.0.0.1:5432:5432", "5353:53/udp"] {
            assert_eq!(s.parse::<PublishedPort>().unwrap().to_string(), s);
        }
        for bad in ["8080", "a:b", "0:80", ":80:80", "1:2:3:4"] {
            assert!(bad.parse::<PublishedPort>().is_err(), "{bad}");
        }
    }

    #[test]
    fn workload_names_round_trip() {
        let n = WorkloadName::new("wl-acme", "dev", 1);
        assert_eq!(n.to_string(), "wl-acme-dev-001");
        assert_eq!("wl-acme-dev-001".parse::<WorkloadName>(), Ok(n));
        assert_eq!("wl-dev-001".parse::<WorkloadName>().unwrap().prefix, "wl");
        assert_eq!("gx-prod-1234".parse::<WorkloadName>().unwrap().seq, 1234);
        for bad in [
            "dev-001",
            "wl-dev-1",
            "wl-dev-000",
            "wl-Dev-001",
            "wl--001",
            "wl-dev-01a",
            "-x-dev-001",
        ] {
            assert!(bad.parse::<WorkloadName>().is_err(), "{bad}");
        }
    }
}
