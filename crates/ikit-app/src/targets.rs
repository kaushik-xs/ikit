use std::io;
use std::time::Duration;

use ikit_core::target::{Check, TargetOrigin};
use ikit_core::{Connection, Target, TargetStatus};

use crate::{App, AppError, Result};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

fn not_found(name: &str) -> AppError {
    AppError::NotFound(format!("target `{name}`"))
}

impl App {
    /// Registers an existing VM or cluster. Use [`App::target_test`] to check it.
    pub async fn target_attach(&self, name: &str, connection: Connection) -> Result<Target> {
        if !ikit_core::is_slug(name) {
            return Err(AppError::Invalid(format!(
                "invalid target name `{name}`: use {}",
                ikit_core::SLUG_RULE
            )));
        }
        validate_connection(&connection)?;
        self.store
            .targets()
            .insert(name, TargetOrigin::Attached, &connection)
            .await
            .map_err(|e| match AppError::from(e) {
                AppError::AlreadyExists(_) => AppError::AlreadyExists(format!("target `{name}`")),
                e => e,
            })
    }

    pub async fn target_list(&self) -> Result<Vec<Target>> {
        Ok(self.store.targets().list().await?)
    }

    pub async fn target_show(&self, name: &str) -> Result<Target> {
        self.store
            .targets()
            .get(name)
            .await?
            .ok_or_else(|| not_found(name))
    }

    /// Checks that the target is reachable and has what workloads need (Docker Compose on a
    /// VM, API access on a cluster), and records the result.
    pub async fn target_test(&self, name: &str) -> Result<Target> {
        let target = self.target_show(name).await?;
        let checks = match &target.connection {
            Connection::Local => self.probe_docker(&[]).await,
            Connection::Ssh { .. } => self.probe_ssh(&target.connection).await,
            Connection::K8s {
                context,
                kubeconfig,
            } => {
                let mut base = vec!["--context".to_owned(), context.clone()];
                if let Some(k) = kubeconfig {
                    base.extend(["--kubeconfig".to_owned(), k.display().to_string()]);
                }
                self.probe_k8s(&base).await
            }
        };
        let status = if checks.iter().all(|c| c.ok) {
            TargetStatus::Ready
        } else {
            TargetStatus::Failed
        };
        self.store
            .targets()
            .record_checks(name, status, &checks)
            .await?;
        self.target_show(name).await
    }

    /// Removes an attached target from ikit without touching the host or cluster.
    pub async fn target_detach(&self, name: &str) -> Result<()> {
        let target = self.target_show(name).await?;
        let users: Vec<String> = self
            .store
            .workloads()
            .list(None)
            .await?
            .into_iter()
            .filter(|w| w.target == target.name)
            .map(|w| w.name)
            .collect();
        if !users.is_empty() {
            return Err(AppError::InUse {
                what: format!("target `{name}`"),
                by: users,
            });
        }
        if target.origin == TargetOrigin::Created {
            return Err(AppError::Invalid(format!(
                "target `{name}` was created by ikit; destroy it instead"
            )));
        }
        self.store.targets().delete(name).await?;
        Ok(())
    }

    /// Runs `program args`, mapping a missing program and failures to a check result.
    async fn check(&self, name: &str, program: &str, args: Vec<String>) -> (Check, Option<String>) {
        let (ok, detail, stdout) = match self.exec.run(program, &args, None, PROBE_TIMEOUT).await {
            Ok(out) if out.success() => (true, String::new(), Some(out.stdout.trim().to_owned())),
            Ok(out) => (false, out.failure_detail(), None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                (false, format!("`{program}` not found on PATH"), None)
            }
            Err(e) => (false, e.to_string(), None),
        };
        (
            Check {
                name: name.to_owned(),
                ok,
                detail,
            },
            stdout,
        )
    }

    /// Docker engine and Compose plugin, run locally or through `ssh_prefix`
    /// (`["ssh", <opts>.., dest]`).
    async fn probe_docker(&self, ssh_prefix: &[String]) -> Vec<Check> {
        let run = |cmd: &str| -> (String, Vec<String>) {
            match ssh_prefix.split_first() {
                None => {
                    let mut parts = cmd.split(' ').map(str::to_owned);
                    (parts.next().unwrap_or_default(), parts.collect())
                }
                Some((ssh, rest)) => {
                    let mut args = rest.to_vec();
                    args.push(cmd.to_owned());
                    (ssh.clone(), args)
                }
            }
        };
        let mut checks = Vec::new();

        let (program, mut args) = run("docker version");
        if ssh_prefix.is_empty() {
            args.extend(["--format".to_owned(), "{{.Server.Version}}".to_owned()]);
        } else {
            args.last_mut()
                .unwrap()
                .push_str(" --format '{{.Server.Version}}'");
        }
        let (mut check, out) = self.check("docker engine", &program, args).await;
        if let Some(v) = out {
            check.detail = format!("server {v}");
        }
        checks.push(check);

        let (program, args) = run("docker compose version --short");
        let (mut check, out) = self.check("docker compose", &program, args).await;
        if let Some(v) = out {
            check.detail = format!("compose {v}");
        }
        checks.push(check);
        checks
    }

    async fn probe_ssh(&self, connection: &Connection) -> Vec<Check> {
        let mut prefix = vec!["ssh".to_owned()];
        prefix.extend(ssh_args(connection));

        let mut args = prefix[1..].to_vec();
        args.push("true".to_owned());
        let (mut check, ok) = self.check("ssh", "ssh", args).await;
        if ok.is_some() {
            check.detail = connection.describe();
        }
        if !check.ok {
            return vec![check];
        }
        let mut checks = vec![check];
        checks.extend(self.probe_docker(&prefix).await);
        checks
    }

    async fn probe_k8s(&self, base: &[String]) -> Vec<Check> {
        let mut args = base.to_vec();
        args.extend(["version", "-o", "json", "--request-timeout=10s"].map(str::to_owned));
        let (mut check, out) = self.check("cluster api", "kubectl", args).await;
        if let Some(json) = out {
            let version = serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| v["serverVersion"]["gitVersion"].as_str().map(str::to_owned));
            check.detail =
                version.map_or_else(|| "reachable".to_owned(), |v| format!("server {v}"));
        }
        if !check.ok {
            return vec![check];
        }

        let mut args = base.to_vec();
        args.extend(["auth", "can-i", "create", "namespaces"].map(str::to_owned));
        let (mut perm, out) = self.check("create namespaces", "kubectl", args).await;
        if out.is_some() {
            perm.detail = "allowed".to_owned();
        } else if perm.detail == "no" || perm.detail.is_empty() {
            perm.detail = "not allowed (ikit creates one namespace per workload)".to_owned();
        }
        vec![check, perm]
    }
}

/// `ssh` arguments up to and including the destination: batch mode (never prompt), a
/// connect timeout, and the target's port and identity file.
pub(crate) fn ssh_args(connection: &Connection) -> Vec<String> {
    let mut args: Vec<String> = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]
        .map(str::to_owned)
        .to_vec();
    if let Connection::Ssh {
        port,
        identity_file,
        ..
    } = connection
    {
        if let Some(p) = port {
            args.extend(["-p".to_owned(), p.to_string()]);
        }
        if let Some(i) = identity_file {
            args.extend(["-i".to_owned(), i.display().to_string()]);
        }
    }
    args.extend(connection.ssh_destination());
    args
}

fn validate_connection(c: &Connection) -> Result<()> {
    let invalid = |m: &str| Err(AppError::Invalid(m.to_owned()));
    match c {
        Connection::Local => Ok(()),
        Connection::Ssh { host, user, .. } => {
            if host.is_empty() || host.starts_with('-') || host.contains(char::is_whitespace) {
                return invalid("invalid ssh host");
            }
            if user.as_deref().is_some_and(|u| {
                u.is_empty() || u.starts_with('-') || u.contains(char::is_whitespace)
            }) {
                return invalid("invalid ssh user");
            }
            Ok(())
        }
        Connection::K8s { context, .. }
            if context.trim().is_empty() || context.starts_with('-') =>
        {
            invalid("invalid kubeconfig context")
        }
        Connection::K8s { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::testing::app;
    use crate::{Exec, ExecOutput};

    /// Replays canned outputs in order and records the commands it was given.
    #[derive(Default)]
    struct FakeExec {
        outputs: Mutex<VecDeque<io::Result<ExecOutput>>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeExec {
        fn new(outputs: Vec<io::Result<ExecOutput>>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Mutex::new(outputs.into()),
                calls: Mutex::default(),
            })
        }
    }

    #[async_trait]
    impl Exec for FakeExec {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            _: Option<&[u8]>,
            _: Duration,
        ) -> io::Result<ExecOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected command")
        }
    }

    fn ok(stdout: &str) -> io::Result<ExecOutput> {
        Ok(ExecOutput {
            code: Some(0),
            stdout: stdout.into(),
            ..Default::default()
        })
    }

    fn fail(stderr: &str) -> io::Result<ExecOutput> {
        Ok(ExecOutput {
            code: Some(255),
            stderr: stderr.into(),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn ssh_target_ready_when_docker_and_compose_work() {
        let (_dir, app, _) = app().await;
        let exec = FakeExec::new(vec![ok(""), ok("27.3.1\n"), ok("2.29.7\n")]);
        let app = app.with_exec(exec.clone());
        let conn = Connection::Ssh {
            host: "10.0.0.5".into(),
            user: Some("ubuntu".into()),
            port: Some(2222),
            identity_file: None,
        };
        app.target_attach("edge", conn).await.unwrap();

        let target = app.target_test("edge").await.unwrap();
        assert_eq!(target.status, TargetStatus::Ready);
        let details: Vec<&str> = target
            .last_checks
            .iter()
            .map(|c| c.detail.as_str())
            .collect();
        assert_eq!(
            details,
            [
                "ssh ubuntu@10.0.0.5:2222",
                "server 27.3.1",
                "compose 2.29.7"
            ]
        );
        let calls = exec.calls.lock().unwrap();
        assert_eq!(
            calls[0],
            "ssh -o BatchMode=yes -o ConnectTimeout=10 -p 2222 ubuntu@10.0.0.5 true"
        );
        assert!(
            calls[1].ends_with("ubuntu@10.0.0.5 docker version --format '{{.Server.Version}}'"),
            "{}",
            calls[1]
        );
    }

    #[tokio::test]
    async fn unreachable_ssh_stops_after_first_check() {
        let (_dir, app, _) = app().await;
        let app = app.with_exec(FakeExec::new(vec![fail(
            "ssh: connect to host h port 22: Connection refused",
        )]));
        app.target_attach(
            "edge",
            Connection::Ssh {
                host: "h".into(),
                user: None,
                port: None,
                identity_file: None,
            },
        )
        .await
        .unwrap();

        let target = app.target_test("edge").await.unwrap();
        assert_eq!(target.status, TargetStatus::Failed);
        assert_eq!(target.last_checks.len(), 1);
        assert!(target.last_checks[0].detail.contains("Connection refused"));
    }

    #[tokio::test]
    async fn local_target_reports_missing_docker() {
        let (_dir, app, _) = app().await;
        let missing = || Err(io::Error::new(io::ErrorKind::NotFound, "no such file"));
        let app = app.with_exec(FakeExec::new(vec![missing(), missing()]));
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();

        let target = app.target_test("laptop").await.unwrap();
        assert_eq!(target.status, TargetStatus::Failed);
        assert_eq!(target.last_checks[0].detail, "`docker` not found on PATH");
    }

    #[tokio::test]
    async fn k8s_target_checks_api_and_permissions() {
        let (_dir, app, _) = app().await;
        let version = r#"{"serverVersion": {"gitVersion": "v1.31.2"}}"#;
        let app = app.with_exec(FakeExec::new(vec![ok(version), ok("yes")]));
        app.target_attach(
            "kind",
            Connection::K8s {
                context: "kind-kind".into(),
                kubeconfig: None,
            },
        )
        .await
        .unwrap();

        let target = app.target_test("kind").await.unwrap();
        assert_eq!(target.status, TargetStatus::Ready);
        assert_eq!(target.last_checks[0].detail, "server v1.31.2");
    }

    #[tokio::test]
    async fn attach_validates_and_rejects_duplicates() {
        let (_dir, app, _) = app().await;
        assert!(matches!(
            app.target_attach("Bad Name", Connection::Local).await,
            Err(AppError::Invalid(_))
        ));
        let bad_host = Connection::Ssh {
            host: "-oProxyCommand=x".into(),
            user: None,
            port: None,
            identity_file: None,
        };
        assert!(matches!(
            app.target_attach("edge", bad_host).await,
            Err(AppError::Invalid(_))
        ));
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        assert!(matches!(
            app.target_attach("laptop", Connection::Local).await,
            Err(AppError::AlreadyExists(_))
        ));
        app.target_detach("laptop").await.unwrap();
        assert!(matches!(
            app.target_show("laptop").await,
            Err(AppError::NotFound(_))
        ));
    }
}
