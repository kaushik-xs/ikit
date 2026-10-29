//! Drift: differences between what was last applied and what actually runs on the target.
//!
//! Compose labels each container with a hash of the service config it was created from, so
//! comparing those labels with `docker compose config --hash` over the applied files finds
//! containers someone changed or recreated by hand. Missing, stopped, unhealthy and orphan
//! containers are reported too. Definition changes not yet applied are not drift; they are
//! counted in `unapplied` and shown by `ikit plan`.

use std::collections::BTreeMap;

use ikit_core::WorkloadStatus;
use ikit_provider::compose::COMPOSE_FILE;
use serde::Serialize;

use crate::deploy::{QUICK, failed, parse_ps};
use crate::{App, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DriftKind {
    /// The workload is applied but its compose files are gone from the target.
    Undeployed,
    /// A service in the applied files has no container.
    Missing,
    /// Its container exists but is not running.
    Stopped,
    Unhealthy,
    /// The container was created from a different config than the applied files.
    Modified,
    /// A container in the project that the applied files do not define.
    Extra,
}

impl std::fmt::Display for DriftKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Undeployed => "undeployed",
            Self::Missing => "missing",
            Self::Stopped => "stopped",
            Self::Unhealthy => "unhealthy",
            Self::Modified => "modified",
            Self::Extra => "extra",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriftItem {
    pub service: String,
    pub kind: DriftKind,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriftReport {
    pub workload: String,
    pub target: String,
    pub status: WorkloadStatus,
    /// Service changes in the definition that `apply` has not made yet.
    pub unapplied: usize,
    pub items: Vec<DriftItem>,
}

impl DriftReport {
    pub fn drifted(&self) -> bool {
        !self.items.is_empty()
    }
}

/// `docker compose config --hash '*'` prints `<service> <hash>` per line.
fn parse_hashes(stdout: &str) -> BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|l| {
            let (svc, hash) = l.trim().split_once(' ')?;
            Some((svc.to_owned(), hash.trim().to_owned()))
        })
        .collect()
}

impl App {
    /// Compares the workload's containers with the files last applied to its target.
    pub async fn deploy_drift(&self, workload: &str) -> Result<DriftReport> {
        let wl = self.workload_get(workload).await?;
        let target = self.target_show(&wl.target).await?;
        let place = self.place(&target, workload)?;
        let unapplied = self.deploy_plan(workload).await?.plan.changes.len();
        let mut report = DriftReport {
            workload: workload.to_owned(),
            target: target.name.clone(),
            status: wl.status,
            unapplied,
            items: vec![],
        };
        if self.read_file(&place, COMPOSE_FILE).await?.is_none() {
            if wl.status == WorkloadStatus::Applied {
                report.items.push(DriftItem {
                    service: "*".into(),
                    kind: DriftKind::Undeployed,
                    detail: format!("the compose files are missing on {}", target.name),
                });
            }
            return Ok(report);
        }

        let out = self
            .compose(&place, workload, &["config", "--hash", "*"], QUICK)
            .await?;
        if !out.success() {
            return Err(failed("docker compose config --hash", &out));
        }
        let expected = parse_hashes(&out.stdout);
        let out = self
            .compose(
                &place,
                workload,
                &["ps", "--all", "--format", "json"],
                QUICK,
            )
            .await?;
        if !out.success() {
            return Err(failed("docker compose ps", &out));
        }
        let containers = parse_ps(&out.stdout);

        let mut item = |service: &str, kind, detail: String| {
            report.items.push(DriftItem {
                service: service.to_owned(),
                kind,
                detail,
            })
        };
        for (service, hash) in &expected {
            let mine: Vec<_> = containers
                .iter()
                .filter(|c| &c.service == service)
                .collect();
            if mine.is_empty() {
                item(service, DriftKind::Missing, "no container".into());
                continue;
            }
            for c in mine {
                if c.state != "running" {
                    item(
                        service,
                        DriftKind::Stopped,
                        format!("{} ({})", c.state, c.status),
                    );
                } else if c.health == "unhealthy" {
                    item(service, DriftKind::Unhealthy, c.status.clone());
                }
                if !c.config_hash.is_empty() && &c.config_hash != hash {
                    item(
                        service,
                        DriftKind::Modified,
                        format!(
                            "container runs {} with a config that differs from the applied files",
                            c.image
                        ),
                    );
                }
            }
        }
        for c in containers
            .iter()
            .filter(|c| !expected.contains_key(&c.service))
        {
            item(
                &c.service,
                DriftKind::Extra,
                format!("{} is not in the applied files", c.image),
            );
        }
        Ok(report)
    }

    /// Drift of every workload in the org that has been applied (or failed to apply).
    pub async fn deploy_drift_all(&self) -> Result<Vec<DriftReport>> {
        let mut reports = Vec::new();
        for wl in self.workload_list(None).await? {
            if wl.status != WorkloadStatus::Draft {
                reports.push(self.deploy_drift(&wl.name).await?);
            }
        }
        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use ikit_core::{Category, Connection};

    use super::*;
    use crate::testing::app;
    use crate::{Exec, ExecOutput, ServiceRequest};

    /// Answers `config --hash` and `ps` with canned output; every other command succeeds.
    #[derive(Default)]
    struct Docker {
        hashes: Mutex<String>,
        ps: Mutex<String>,
    }

    #[async_trait]
    impl Exec for Docker {
        async fn run(
            &self,
            _: &str,
            args: &[String],
            _: Option<&[u8]>,
            _: Duration,
        ) -> io::Result<ExecOutput> {
            let line = args.join(" ");
            let stdout = if line.contains("config --hash") {
                self.hashes.lock().unwrap().clone()
            } else if line.contains(" ps ") {
                self.ps.lock().unwrap().clone()
            } else {
                String::new()
            };
            Ok(ExecOutput {
                code: Some(0),
                stdout,
                ..Default::default()
            })
        }
    }

    fn ps(service: &str, state: &str, health: &str, hash: &str) -> String {
        serde_json::json!({
            "Service": service, "Image": format!("{service}:1"), "State": state,
            "Health": health, "Status": "Up 1 minute",
            "Labels": format!("com.docker.compose.service={service},com.docker.compose.config-hash={hash},x=a,b"),
        })
        .to_string()
    }

    #[tokio::test]
    async fn finds_modified_stopped_missing_and_extra_containers() {
        let (_dir, app, _) = app().await;
        let docker = Arc::new(Docker::default());
        let app = app.with_exec(docker.clone());
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        for svc in ["redis", "nginx"] {
            let req = ServiceRequest {
                name: None,
                registry: svc.into(),
                constraint: "latest".parse().unwrap(),
                group: Category::Data,
                env: BTreeMap::new(),
            };
            app.services_add("wl-acme-dev-001", vec![req])
                .await
                .unwrap();
        }
        let wl = "wl-acme-dev-001";
        let none = app.deploy_drift(wl).await.unwrap();
        assert!(
            !none.drifted(),
            "a draft with nothing deployed has no drift"
        );
        assert_eq!(none.unapplied, 2);

        let plan = app.deploy_plan(wl).await.unwrap();
        app.deploy_apply(&plan, "test", false).await.unwrap();
        *docker.hashes.lock().unwrap() = "nginx aaa\nredis bbb\n".into();
        *docker.ps.lock().unwrap() = [
            ps("nginx", "running", "", "aaa"),
            ps("redis", "running", "healthy", "bbb"),
        ]
        .join("\n");
        let clean = app.deploy_drift(wl).await.unwrap();
        assert!(!clean.drifted() && clean.unapplied == 0, "{clean:?}");

        *docker.ps.lock().unwrap() = [
            ps("nginx", "exited", "", "zzz"),
            ps("adminer", "running", "", "ccc"),
        ]
        .join("\n");
        let drift = app.deploy_drift(wl).await.unwrap();
        let kinds: Vec<_> = drift
            .items
            .iter()
            .map(|i| (i.service.as_str(), i.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("nginx", DriftKind::Stopped),
                ("nginx", DriftKind::Modified),
                ("redis", DriftKind::Missing),
                ("adminer", DriftKind::Extra),
            ]
        );

        std::fs::remove_dir_all(app.org().dir.join("deploy").join(wl)).unwrap();
        let gone = app.deploy_drift_all().await.unwrap();
        assert_eq!(gone[0].items[0].kind, DriftKind::Undeployed);
    }
}
