//! Docker networks outside a workload that one of its service groups joins as
//! `external: true`, such as a network another Compose project owns. ikit never creates
//! or removes them; `plan` checks that they exist on the target.

use ikit_core::{Category, DockerNetwork, Workload, is_docker_network_name};

use crate::deploy::{Place, QUICK, failed, sh};
use crate::{App, AppError, Result};

impl App {
    /// Has `group` of `workload` join the Docker network `name` from the next apply.
    pub async fn docker_network_attach(
        &self,
        workload: &str,
        name: &str,
        group: Category,
    ) -> Result<Vec<DockerNetwork>> {
        if !is_docker_network_name(name) {
            return Err(AppError::Invalid(format!(
                "`{name}` is not a valid Docker network name (letters, digits, `_`, `.`, `-`; \
                 `app`, `platform` and `data` are the groups' own networks)"
            )));
        }
        let wl = self.workload_get(workload).await?;
        if !self
            .store
            .networks()
            .attach_docker(wl.id, name, group)
            .await?
        {
            return Err(AppError::AlreadyExists(format!(
                "Docker network `{name}` on the {group} group of {workload}"
            )));
        }
        Ok(self.store.networks().docker_networks(wl.id).await?)
    }

    /// Detaches `name` from `group`, or from every group with `None`.
    pub async fn docker_network_detach(
        &self,
        workload: &str,
        name: &str,
        group: Option<Category>,
    ) -> Result<Vec<DockerNetwork>> {
        let wl = self.workload_get(workload).await?;
        if self
            .store
            .networks()
            .detach_docker(wl.id, name, group)
            .await?
            == 0
        {
            let on = group
                .map(|g| format!(" on the {g} group"))
                .unwrap_or_default();
            return Err(AppError::NotFound(format!(
                "Docker network `{name}`{on} of {workload}"
            )));
        }
        Ok(self.store.networks().docker_networks(wl.id).await?)
    }

    pub async fn docker_networks(&self, workload: &str) -> Result<Vec<DockerNetwork>> {
        let wl = self.workload_get(workload).await?;
        Ok(self.store.networks().docker_networks(wl.id).await?)
    }

    /// One blocker per attached network missing on the target. Networks of groups without
    /// services are left out of the compose file, so they are not checked either.
    pub(crate) async fn docker_network_blockers(
        &self,
        wl: &Workload,
        place: &Place,
    ) -> Result<Vec<String>> {
        let attached = self.store.networks().docker_networks(wl.id).await?;
        if attached.is_empty() {
            return Ok(vec![]);
        }
        let services = self.store.workloads().services(wl.id).await?;
        let mut wanted: Vec<&str> = attached
            .iter()
            .filter(|n| services.iter().any(|s| s.group == n.group))
            .map(|n| n.name.as_str())
            .collect();
        wanted.dedup();
        if wanted.is_empty() {
            return Ok(vec![]);
        }
        let existing = self.docker_network_names(place).await?;
        Ok(wanted
            .into_iter()
            .filter(|n| !existing.iter().any(|e| e == n))
            .map(|n| {
                format!(
                    "Docker network `{n}` does not exist on `{}`; create it (`docker network create {n}`) \
                     or start the project that owns it, or detach it with `ikit net docker detach {n} --wl {}`",
                    wl.target, wl.name
                )
            })
            .collect())
    }

    /// The names of the Docker networks on the target.
    async fn docker_network_names(&self, place: &Place) -> Result<Vec<String>> {
        let out = match place {
            Place::Local(_) => {
                let args = ["network", "ls", "--format", "{{.Name}}"].map(str::to_owned);
                self.exec.run("docker", &args, None, QUICK).await?
            }
            Place::Ssh { args, .. } => {
                let script = format!("docker network ls --format {}", sh("{{.Name}}"));
                self.ssh(args, &script, None, QUICK).await?
            }
        };
        if !out.success() {
            return Err(failed("listing Docker networks", &out));
        }
        Ok(out.stdout.lines().map(|l| l.trim().to_owned()).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use ikit_core::Connection;

    use super::*;
    use crate::testing::app;
    use crate::{Exec, ExecOutput, ServiceRequest};

    /// Answers `docker network ls` with the networks it holds.
    #[derive(Default)]
    struct FakeNetworks(Mutex<Vec<String>>);

    #[async_trait]
    impl Exec for FakeNetworks {
        async fn run(
            &self,
            _program: &str,
            args: &[String],
            _stdin: Option<&[u8]>,
            _timeout: Duration,
        ) -> io::Result<ExecOutput> {
            let stdout = if args.starts_with(&["network".to_owned(), "ls".to_owned()]) {
                self.0.lock().unwrap().join("\n")
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

    #[tokio::test]
    async fn attached_networks_are_rendered_and_must_exist() {
        let (_dir, app, _) = app().await;
        let docker = Arc::new(FakeNetworks::default());
        let app = app.with_exec(docker.clone());
        app.target_attach("laptop", Connection::Local)
            .await
            .unwrap();
        let wl = "wl-acme-dev-001";
        app.workload_create("dev", "laptop").await.unwrap();
        let redis = ServiceRequest {
            name: None,
            registry: "redis".into(),
            constraint: "7".parse().unwrap(),
            group: Category::Data,
            env: BTreeMap::new(),
        };
        app.services_add(wl, vec![redis]).await.unwrap();

        for bad in ["data", "-x", "has space"] {
            let e = app
                .docker_network_attach(wl, bad, Category::Data)
                .await
                .unwrap_err();
            assert!(matches!(e, AppError::Invalid(_)), "{bad}: {e}");
        }
        app.docker_network_attach(wl, "shared-db", Category::Data)
            .await
            .unwrap();
        assert!(matches!(
            app.docker_network_attach(wl, "shared-db", Category::Data)
                .await,
            Err(AppError::AlreadyExists(_))
        ));
        // The app group has no services, so its network is neither rendered nor checked.
        app.docker_network_attach(wl, "edge", Category::App)
            .await
            .unwrap();

        let plan = app.deploy_plan(wl).await.unwrap();
        assert_eq!(plan.blockers.len(), 1, "{:?}", plan.blockers);
        assert!(
            plan.blockers[0].contains("`shared-db` does not exist on `laptop`"),
            "{}",
            plan.blockers[0]
        );
        let compose = &plan.rendered.compose;
        assert!(compose.contains("external: true"), "{compose}");
        assert!(compose.contains("wl-acme-dev-001-redis"), "{compose}");
        assert!(!compose.contains("edge"), "{compose}");

        docker.0.lock().unwrap().push("shared-db".into());
        assert!(app.deploy_plan(wl).await.unwrap().blockers.is_empty());

        assert!(matches!(
            app.docker_network_detach(wl, "shared-db", Some(Category::App))
                .await,
            Err(AppError::NotFound(_))
        ));
        let left = app
            .docker_network_detach(wl, "shared-db", None)
            .await
            .unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name, "edge");
    }
}
