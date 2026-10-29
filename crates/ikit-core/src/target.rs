//! Targets: the VM or Kubernetes cluster a workload runs on.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

string_enum! {
    "target kind",
    pub enum TargetKind {
        Vm => "vm",
        K8s => "k8s",
    }
}

string_enum! {
    "target origin",
    pub enum TargetOrigin {
        /// Provisioned by ikit; ikit may destroy it.
        Created => "created",
        /// An existing host or cluster; ikit only detaches it.
        Attached => "attached",
    }
}

string_enum! {
    "target status",
    pub enum TargetStatus {
        /// Never tested.
        Unknown => "unknown",
        Ready => "ready",
        /// The last `target test` failed.
        Failed => "failed",
    }
}

/// How ikit reaches a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Connection {
    /// The machine ikit runs on.
    Local,
    /// A VM reached with the system `ssh`, so `~/.ssh/config` and ssh-agent apply.
    Ssh {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<PathBuf>,
    },
    /// A cluster reached through a kubeconfig context.
    K8s {
        context: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kubeconfig: Option<PathBuf>,
    },
}

impl Connection {
    pub fn kind(&self) -> TargetKind {
        match self {
            Self::Local | Self::Ssh { .. } => TargetKind::Vm,
            Self::K8s { .. } => TargetKind::K8s,
        }
    }

    /// `[user@]host`, for ssh.
    pub fn ssh_destination(&self) -> Option<String> {
        match self {
            Self::Ssh {
                host,
                user: Some(user),
                ..
            } => Some(format!("{user}@{host}")),
            Self::Ssh {
                host, user: None, ..
            } => Some(host.clone()),
            _ => None,
        }
    }

    /// Short human-readable form, e.g. `ssh ubuntu@10.0.0.5:2222` or `k8s kind-kind`.
    pub fn describe(&self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::Ssh { port, .. } => {
                let dest = self.ssh_destination().unwrap_or_default();
                match port {
                    Some(p) => format!("ssh {dest}:{p}"),
                    None => format!("ssh {dest}"),
                }
            }
            Self::K8s { context, .. } => format!("context {context}"),
        }
    }
}

/// Parses `[user@]host[:port]`.
pub fn parse_ssh_destination(s: &str) -> Result<(Option<String>, String, Option<u16>), String> {
    let (user, rest) = match s.rsplit_once('@') {
        Some((u, r)) if !u.is_empty() => (Some(u.to_owned()), r),
        Some(_) => return Err(format!("invalid ssh destination `{s}`")),
        None => (None, s),
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !rest.starts_with('[') => {
            let port = p
                .parse::<u16>()
                .map_err(|_| format!("invalid ssh port `{p}`"))?;
            (h, Some(port))
        }
        _ => (rest, None),
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(format!("invalid ssh destination `{s}`"));
    }
    Ok((user, host.to_owned(), port))
}

/// One check run by `ikit target test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub id: i64,
    pub name: String,
    pub kind: TargetKind,
    pub origin: TargetOrigin,
    pub connection: Connection,
    pub status: TargetStatus,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_checks: Vec<Check>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_destinations() {
        assert_eq!(
            parse_ssh_destination("host"),
            Ok((None, "host".into(), None))
        );
        assert_eq!(
            parse_ssh_destination("ubuntu@10.147.17.5:2222"),
            Ok((Some("ubuntu".into()), "10.147.17.5".into(), Some(2222)))
        );
        assert!(parse_ssh_destination("@host").is_err());
        assert!(parse_ssh_destination("host:notaport").is_err());
    }

    #[test]
    fn connection_json_is_tagged() {
        let c = Connection::Ssh {
            host: "h".into(),
            user: Some("u".into()),
            port: None,
            identity_file: None,
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"type":"ssh","host":"h","user":"u"}"#
        );
        assert_eq!(c.kind(), TargetKind::Vm);
        assert_eq!(
            Connection::K8s {
                context: "c".into(),
                kubeconfig: None
            }
            .kind(),
            TargetKind::K8s
        );
    }
}
