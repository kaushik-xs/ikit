use std::io;
use std::time::Duration;

use ikit_core::target::{Check, TargetOrigin};
use ikit_core::{Connection, Target, TargetStatus, ZtRoute};

use crate::zerotier::{member_address, zt_route};
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

    /// Registers a VM reached over SSH at its ZeroTier address: authorizes `node` on
    /// `network` (it may not have joined yet) and uses the address Central assigns it, so
    /// the VM's SSH port never has to be public.
    pub async fn target_attach_zt(&self, name: &str, ssh: ZtSsh) -> Result<Target> {
        if !ikit_core::is_slug(name) {
            return Err(AppError::Invalid(format!(
                "invalid target name `{name}`: use {}",
                ikit_core::SLUG_RULE
            )));
        }
        if self.store.targets().get(name).await?.is_some() {
            return Err(AppError::AlreadyExists(format!("target `{name}`")));
        }
        let network = self.zt_show_network(&ssh.network).await?;
        let (_, ip) = self
            .zt_authorize_with_address(&network, &ssh.node, name)
            .await?;
        let connection = Connection::Ssh {
            host: ip.to_string(),
            user: ssh.user,
            port: ssh.port,
            identity_file: ssh.identity_file,
            zerotier: Some(ZtRoute {
                network: network.name,
                node: ssh.node,
            }),
        };
        self.target_attach(name, connection).await
    }

    /// Moves an SSH target onto ZeroTier: joins it to `network` over its current
    /// connection, authorizes it, and from then on reaches it at its ZeroTier address.
    pub async fn target_use_zt(&self, name: &str, network: &str) -> Result<Target> {
        let target = self.target_show(name).await?;
        let Connection::Ssh {
            user,
            port,
            identity_file,
            ..
        } = &target.connection
        else {
            return Err(AppError::Invalid(format!(
                "target `{name}` is {}; only SSH targets can be reached over ZeroTier",
                target.connection.describe()
            )));
        };
        let joined = self.zt_join(network, Some(name), false, None).await?;
        let network = self.zt_show_network(network).await?;
        let (_, ip) = self
            .zt_authorize_with_address(&network, &joined.node_id, name)
            .await?;
        let connection = Connection::Ssh {
            host: ip.to_string(),
            user: user.clone(),
            port: *port,
            identity_file: identity_file.clone(),
            zerotier: Some(ZtRoute {
                network: network.name,
                node: joined.node_id,
            }),
        };
        validate_connection(&connection)?;
        self.store
            .targets()
            .update_connection(name, &connection)
            .await?;
        self.target_show(name).await
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
        let mut target = self.target_show(name).await?;
        let mut checks = Vec::new();
        if zt_route(&target).is_some() {
            let (check, connection) = self.probe_zerotier(&target).await;
            let ok = check.ok;
            checks.push(check);
            if let Some(c) = connection {
                self.store.targets().update_connection(name, &c).await?;
                target.connection = c;
            }
            if !ok {
                self.store
                    .targets()
                    .record_checks(name, TargetStatus::Failed, &checks)
                    .await?;
                return self.target_show(name).await;
            }
        }
        checks.extend(match &target.connection {
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
        });
        if checks.iter().all(|c| c.ok)
            && let Some(check) = self.edge_check(&target).await
        {
            checks.push(check);
        }
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

    /// Looks the target's node up in ZeroTier Central: it must be authorized and have an
    /// address. Returns the connection with that address when it changed.
    async fn probe_zerotier(&self, target: &Target) -> (Check, Option<Connection>) {
        let route = zt_route(target).expect("a ZeroTier route");
        let check = |ok: bool, detail: String| Check {
            name: "zerotier".to_owned(),
            ok,
            detail,
        };
        let network = match self.zt_show_network(&route.network).await {
            Ok(n) => n,
            Err(e) => return (check(false, e.to_string()), None),
        };
        let member = match self.zt_member(&network, &route.node).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                return (
                    check(
                        false,
                        format!("node {} is not a member of `{}`", route.node, network.name),
                    ),
                    None,
                );
            }
            Err(e) => return (check(false, e.to_string()), None),
        };
        if !member.authorized {
            return (
                check(
                    false,
                    format!(
                        "node {} is not authorized on `{}` (`ikit net zt authorize {} {}`)",
                        route.node, network.name, network.name, route.node
                    ),
                ),
                None,
            );
        }
        let Some(ip) = member_address(&member) else {
            return (
                check(
                    false,
                    format!("node {} has no address on `{}`", route.node, network.name),
                ),
                None,
            );
        };
        let online = if member.online { "online" } else { "offline" };
        let mut connection = target.connection.clone();
        let Connection::Ssh { host, .. } = &mut connection else {
            unreachable!("a ZeroTier route is only set on SSH targets");
        };
        if *host == ip.to_string() {
            return (
                check(true, format!("node {} at {ip}, {online}", route.node)),
                None,
            );
        }
        let detail = format!("node {} moved {host} → {ip}, {online}", route.node);
        *host = ip.to_string();
        (check(true, detail), Some(connection))
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

/// An SSH target reached at its ZeroTier address.
#[derive(Debug, Clone)]
pub struct ZtSsh {
    /// The ikit network name.
    pub network: String,
    /// The VM's 10-digit node ID (`zerotier-cli info` on the VM).
    pub node: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<std::path::PathBuf>,
}

fn validate_connection(c: &Connection) -> Result<()> {
    let invalid = |m: &str| Err(AppError::Invalid(m.to_owned()));
    match c {
        Connection::Local => Ok(()),
        Connection::Ssh {
            zerotier: Some(zt), ..
        } if !ikit_net::is_node_id(&zt.node) || !ikit_core::is_slug(&zt.network) => {
            invalid("invalid ZeroTier route: expected an ikit network name and a 10-digit node ID")
        }
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
            zerotier: None,
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
                zerotier: None,
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
    async fn zerotier_targets_are_authorized_and_follow_their_address() {
        let (_dir, app, _) = app().await;
        let info = r#"{"address":"a1b2c3d4e5","online":true,"version":"1.14.2"}"#;
        let exec = FakeExec::new(vec![
            // attach_zt with --no-test: nothing runs on the VM.
            // test: ssh, docker, compose over the ZeroTier address.
            ok(""),
            ok("27.3.1\n"),
            ok("2.29.7\n"),
            // use_zt on `edge`: zerotier-cli join, then info, over the public address.
            ok(""),
            ok(info),
        ]);
        let app = app.with_exec(exec.clone());
        crate::testing::zt_profile(&app).await;
        app.zt_create("dev-net", "zt", None, None).await.unwrap();

        let ssh = |node: &str| ZtSsh {
            network: "dev-net".into(),
            node: node.into(),
            user: Some("ubuntu".into()),
            port: None,
            identity_file: None,
        };
        assert!(app.target_attach_zt("vm", ssh("nope")).await.is_err());
        let t = app.target_attach_zt("vm", ssh("0123456789")).await.unwrap();
        assert_eq!(
            t.connection.describe(),
            "ssh ubuntu@10.147.17.5 via zerotier dev-net"
        );
        let member = &app.zt_members("dev-net").await.unwrap()[0];
        assert!(member.authorized);
        assert_eq!(member.name, "vm");
        assert!(matches!(
            app.target_attach_zt("vm", ssh("0123456789")).await,
            Err(AppError::AlreadyExists(_))
        ));

        // A stale address is refreshed from Central before connecting.
        let mut stale = t.connection.clone();
        if let Connection::Ssh { host, .. } = &mut stale {
            *host = "10.147.17.9".into();
        }
        app.store
            .targets()
            .update_connection("vm", &stale)
            .await
            .unwrap();
        let t = app.target_test("vm").await.unwrap();
        assert_eq!(t.status, TargetStatus::Ready, "{:?}", t.last_checks);
        assert_eq!(t.last_checks[0].name, "zerotier");
        assert!(
            t.last_checks[0]
                .detail
                .contains("10.147.17.9 → 10.147.17.5"),
            "{}",
            t.last_checks[0].detail
        );
        assert!(exec.calls.lock().unwrap()[0].contains("ubuntu@10.147.17.5 true"));

        // A deauthorized node fails before ssh is tried.
        app.zt_authorize("dev-net", "0123456789", false, None)
            .await
            .unwrap();
        let t = app.target_test("vm").await.unwrap();
        assert_eq!(t.status, TargetStatus::Failed);
        assert_eq!(t.last_checks.len(), 1);
        assert!(t.last_checks[0].detail.contains("not authorized"));

        // The network cannot be removed while a target is reached through it.
        assert!(matches!(
            app.zt_remove("dev-net", false).await,
            Err(AppError::InUse { .. })
        ));

        // An existing SSH target moves onto ZeroTier.
        let public = Connection::Ssh {
            host: "203.0.113.7".into(),
            user: Some("ubuntu".into()),
            port: Some(2222),
            identity_file: None,
            zerotier: None,
        };
        app.target_attach("edge", public).await.unwrap();
        let t = app.target_use_zt("edge", "dev-net").await.unwrap();
        let Connection::Ssh {
            host,
            port,
            zerotier,
            ..
        } = &t.connection
        else {
            panic!("still ssh");
        };
        assert_eq!(host, "10.147.17.5");
        assert_eq!(*port, Some(2222));
        assert_eq!(zerotier.as_ref().unwrap().node, "a1b2c3d4e5");
        let join = exec.calls.lock().unwrap()[3].clone();
        assert!(join.contains("ubuntu@203.0.113.7") && join.contains("'join'"));

        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        assert!(app.target_use_zt("laptop", "dev-net").await.is_err());
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
            zerotier: None,
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
