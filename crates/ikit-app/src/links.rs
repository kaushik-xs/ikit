//! Links between workloads on one target: a consumer (say an app workload) reaches a
//! provider's (its platform workload's) app and platform services by `service:` references.
//!
//! The provider offers a Docker network, `<provider>-link`, that ikit creates on the target
//! before either side is applied. The provider's app and platform services join it as
//! `<provider>-<service>`, which is unique on the engine however many envs share it; the
//! consumer's app and platform services join it too. Data services never do.

use ikit_core::Workload;
use ikit_provider::compose::link_network_name;
use serde::Serialize;

use crate::deploy::{Place, QUICK, failed, sh};
use crate::{App, AppError, Result};

/// A workload's links, both ways.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WorkloadLinks {
    /// Workloads this one reaches.
    pub links_to: Vec<String>,
    /// Workloads that reach this one.
    pub linked_from: Vec<String>,
}

impl App {
    /// Links `consumer` to `provider`. Returns false if it already was.
    pub async fn workload_link(&self, consumer: &str, provider: &str) -> Result<bool> {
        let c = self.workload_get(consumer).await?;
        let p = self.workload_get(provider).await?;
        if c.id == p.id {
            return Err(AppError::Invalid(format!(
                "{consumer} cannot link to itself"
            )));
        }
        if c.target != p.target {
            return Err(AppError::Invalid(format!(
                "{consumer} runs on `{}` and {provider} on `{}`; linked workloads share a Docker engine, so they must be on the same target",
                c.target, p.target
            )));
        }
        let nets = self.store.networks();
        if nets.providers_of(p.id).await?.contains(&c.name) {
            return Err(AppError::Invalid(format!(
                "{provider} already links to {consumer}; links go one way"
            )));
        }
        let added = nets.link_workload(c.id, p.id).await?;
        if added {
            self.write_spec(consumer).await?;
            self.write_spec(provider).await?;
        }
        Ok(added)
    }

    /// Returns whether the link existed.
    pub async fn workload_unlink(&self, consumer: &str, provider: &str) -> Result<bool> {
        let c = self.workload_get(consumer).await?;
        let p = self.workload_get(provider).await?;
        let removed = self.store.networks().unlink_workload(c.id, p.id).await?;
        if removed {
            self.write_spec(consumer).await?;
            self.write_spec(provider).await?;
        }
        Ok(removed)
    }

    pub async fn workload_links(&self, workload: &str) -> Result<WorkloadLinks> {
        let wl = self.workload_get(workload).await?;
        let nets = self.store.networks();
        Ok(WorkloadLinks {
            links_to: nets.providers_of(wl.id).await?,
            linked_from: nets.consumers_of(wl.id).await?,
        })
    }

    /// The link networks `wl`'s compose file names: its own when others link to it, and
    /// those of the workloads it links to.
    pub(crate) async fn link_networks(&self, wl: &Workload) -> Result<Vec<String>> {
        let nets = self.store.networks();
        let mut names = Vec::new();
        if !nets.consumers_of(wl.id).await?.is_empty() {
            names.push(link_network_name(&wl.name));
        }
        names.extend(
            nets.providers_of(wl.id)
                .await?
                .iter()
                .map(|p| link_network_name(p)),
        );
        Ok(names)
    }

    /// Creates the link networks `wl` needs that the target does not have yet.
    pub(crate) async fn ensure_link_networks(&self, wl: &Workload, place: &Place) -> Result<()> {
        let wanted = self.link_networks(wl).await?;
        if wanted.is_empty() {
            return Ok(());
        }
        let existing = self.docker_network_names(place).await?;
        for name in wanted.iter().filter(|n| !existing.contains(n)) {
            let label = |k: &str, v: &str| format!("{k}={v}");
            let args = vec![
                "network".to_owned(),
                "create".to_owned(),
                "--label".to_owned(),
                label("ikit.org", &self.org.name),
                "--label".to_owned(),
                label("ikit.link", name),
                name.clone(),
            ];
            let out = match place {
                Place::Local(_) => self.exec.run("docker", &args, None, QUICK).await?,
                Place::Ssh { args: ssh, .. } => {
                    let script = std::iter::once("docker".to_owned())
                        .chain(args.iter().map(|a| sh(a)))
                        .collect::<Vec<_>>()
                        .join(" ");
                    self.ssh(ssh, &script, None, QUICK).await?
                }
            };
            if !out.success() {
                return Err(failed(&format!("creating Docker network {name}"), &out));
            }
        }
        Ok(())
    }

    /// Plan notes about links: workloads this one links to that were never applied, so
    /// their services do not answer yet.
    pub(crate) async fn link_notes(&self, wl: &Workload) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        for p in self.store.networks().providers_of(wl.id).await? {
            let provider = self.workload_get(&p).await?;
            if provider.status != ikit_core::WorkloadStatus::Applied {
                notes.push(format!(
                    "{} links to {p}, which is {}; apply {p} too, or its services will not answer",
                    wl.name, provider.status
                ));
            }
        }
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ikit_core::{Category, Connection};

    use crate::testing::app;
    use crate::{AppError, ServiceRequest};

    fn redis(group: Category) -> ServiceRequest {
        ServiceRequest {
            name: None,
            registry: "redis".into(),
            constraint: "7".parse().unwrap(),
            group,
            env: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn links_are_one_way_on_one_target_and_render_both_sides() {
        let (_dir, app, _) = app().await;
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        app.target_attach("other", Connection::Local).await.unwrap();
        let (a, p, far) = ("wl-acme-dev-001", "wl-acme-dev-002", "wl-acme-dev-003");
        app.workload_create("dev", "laptop").await.unwrap();
        app.workload_create("dev", "laptop").await.unwrap();
        app.workload_create("dev", "other").await.unwrap();
        app.services_add(a, vec![redis(Category::App)])
            .await
            .unwrap();
        app.services_add(p, vec![redis(Category::Platform)])
            .await
            .unwrap();

        assert!(matches!(
            app.workload_link(a, a).await,
            Err(AppError::Invalid(_))
        ));
        let e = app.workload_link(a, far).await.unwrap_err().to_string();
        assert!(e.contains("same target"), "{e}");
        assert!(app.workload_link(a, p).await.unwrap());
        assert!(!app.workload_link(a, p).await.unwrap());
        let e = app.workload_link(p, a).await.unwrap_err().to_string();
        assert!(e.contains("one way"), "{e}");

        let links = app.workload_links(p).await.unwrap();
        assert_eq!(links.linked_from, [a]);
        let provider = app.render_workload(p).await.unwrap().compose;
        assert!(provider.contains("wl-acme-dev-002-link"), "{provider}");
        assert!(provider.contains("wl-acme-dev-002-redis"), "{provider}");
        let consumer = app.render_workload(a).await.unwrap().compose;
        assert!(consumer.contains("wl-acme-dev-002-link"), "{consumer}");
        let notes = app.deploy_plan(a).await.unwrap().warnings;
        assert!(
            notes.iter().any(|n| n.contains("apply wl-acme-dev-002")),
            "{notes:?}"
        );

        assert!(app.workload_unlink(a, p).await.unwrap());
        assert!(
            !app.render_workload(a)
                .await
                .unwrap()
                .compose
                .contains("-link")
        );
    }
}
