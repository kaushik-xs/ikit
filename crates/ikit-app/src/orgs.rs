//! Orgs: each organization has its own folder with its own state database, config,
//! registry, targets, profiles and workloads.

use std::fs;
use std::path::PathBuf;

use ikit_core::Config;
use ikit_core::registry::{ImportMode, Source};
use ikit_core::workload::WorkloadName;
use ikit_state::{SqliteStore, StateStore};
use serde::Serialize;

use crate::context::Context;
use crate::{AppError, Paths, Result, SecretStore};

#[derive(Debug, Clone, Default)]
pub struct NewOrg {
    pub name: String,
    pub display_name: Option<String>,
    /// Workload name prefix; defaults to `wl-<name>`.
    pub prefix: Option<String>,
    /// Defaults to dev, qa, stg, prod.
    pub envs: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrgSummary {
    pub name: String,
    pub display_name: Option<String>,
    pub prefix: String,
    pub envs: Vec<String>,
    pub workloads: usize,
    pub active: bool,
    pub dir: PathBuf,
}

fn not_found(name: &str) -> AppError {
    AppError::NotFound(format!("org `{name}` (see `ikit org list`)"))
}

/// Names of orgs under the home, sorted.
fn org_names(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.orgs_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() && paths.org(&name).state_db().exists() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// An org's `config.yaml` (defaults if the file is missing).
pub fn org_config(paths: &Paths, name: &str) -> Result<Config> {
    let path = paths.org(name).config_file();
    if !path.exists() {
        return Ok(Config::default());
    }
    let yaml = fs::read_to_string(&path)?;
    let config: Config = serde_yaml_ng::from_str(&yaml).map_err(|source| AppError::Config {
        path: path.clone(),
        source,
    })?;
    config
        .validate()
        .map_err(|e| AppError::Invalid(format!("{}: {e}", path.display())))?;
    Ok(config)
}

pub(crate) fn write_org_config(paths: &Paths, name: &str, config: &Config) -> Result<()> {
    let yaml = serde_yaml_ng::to_string(config).expect("config serializes");
    fs::write(paths.org(name).config_file(), yaml)?;
    Ok(())
}

/// Opens (migrating) an org's store and adds any missing built-in registry services.
/// Returns the store with what was migrated and seeded.
pub(crate) async fn open_and_seed(
    paths: &Paths,
    name: &str,
) -> Result<(SqliteStore, ikit_state::OpenReport, Vec<String>)> {
    let (store, report) = SqliteStore::open(&paths.org(name).state_db()).await?;
    let seeds = crate::registry::seeds();
    let seeded = store
        .registry()
        .import(&seeds.services, ImportMode::InsertMissing, Source::Seed)
        .await?
        .versions_added;
    Ok((store, report, seeded))
}

/// Creates an org folder with its config and a seeded state database. The first org
/// created becomes the active one.
pub async fn org_create(paths: &Paths, req: NewOrg) -> Result<OrgSummary> {
    if !ikit_core::is_slug(&req.name) {
        return Err(AppError::Invalid(format!(
            "invalid org name `{}`: use {}",
            req.name,
            ikit_core::SLUG_RULE
        )));
    }
    if paths.legacy_state_db().exists() {
        return Err(AppError::Invalid(
            "this ikit home predates orgs; run `ikit init` first to move it into the `default` org"
                .to_owned(),
        ));
    }
    let org = paths.org(&req.name);
    if org.dir.exists() {
        return Err(AppError::AlreadyExists(format!("org `{}`", req.name)));
    }

    let mut config = Config::for_org(&req.name);
    config.display_name = req.display_name;
    if let Some(prefix) = req.prefix {
        config.prefix = prefix;
    }
    if let Some(envs) = req.envs {
        config.envs = envs;
    }
    config.validate().map_err(AppError::Invalid)?;
    // Prefixes are unique so a workload name always identifies its org.
    for other in org_names(paths)? {
        if org_config(paths, &other)?.prefix == config.prefix {
            return Err(AppError::AlreadyExists(format!(
                "workload prefix `{}` (used by org `{other}`)",
                config.prefix
            )));
        }
    }

    fs::create_dir_all(&org.dir)?;
    let created = async {
        write_org_config(paths, &req.name, &config)?;
        open_and_seed(paths, &req.name).await?;
        Ok::<_, AppError>(())
    }
    .await;
    if let Err(e) = created {
        let _ = fs::remove_dir_all(&org.dir);
        return Err(e);
    }

    let mut ctx = Context::load(paths)?;
    if ctx.org.is_none() {
        ctx.org = Some(req.name.clone());
        ctx.save(paths)?;
    }
    summary(paths, &req.name, &ctx).await
}

async fn summary(paths: &Paths, name: &str, ctx: &Context) -> Result<OrgSummary> {
    let config = org_config(paths, name)?;
    let (store, _) = SqliteStore::open(&paths.org(name).state_db()).await?;
    let workloads = store.workloads().list(None).await?.len();
    Ok(OrgSummary {
        name: name.to_owned(),
        display_name: config.display_name,
        prefix: config.prefix,
        envs: config.envs,
        workloads,
        active: ctx.org.as_deref() == Some(name),
        dir: paths.org(name).dir,
    })
}

pub async fn org_list(paths: &Paths) -> Result<Vec<OrgSummary>> {
    let ctx = Context::load(paths)?;
    let mut out = Vec::new();
    for name in org_names(paths)? {
        out.push(summary(paths, &name, &ctx).await?);
    }
    Ok(out)
}

pub async fn org_show(paths: &Paths, name: &str) -> Result<OrgSummary> {
    if !paths.org(name).state_db().exists() {
        return Err(not_found(name));
    }
    summary(paths, name, &Context::load(paths)?).await
}

/// Makes `name` the active org.
pub fn org_use(paths: &Paths, name: &str) -> Result<()> {
    if !paths.org(name).state_db().exists() {
        return Err(not_found(name));
    }
    let mut ctx = Context::load(paths)?;
    ctx.org = Some(name.to_owned());
    ctx.save(paths)
}

/// Deletes an org's folder (its database, registry, targets and profiles) and its keychain
/// entries. Refused while the org has workloads.
pub async fn org_remove(paths: &Paths, name: &str, secrets: &dyn SecretStore) -> Result<()> {
    if !paths.org(name).state_db().exists() {
        return Err(not_found(name));
    }
    let (store, _) = SqliteStore::open(&paths.org(name).state_db()).await?;
    let workloads: Vec<String> = store
        .workloads()
        .list(None)
        .await?
        .into_iter()
        .map(|w| w.name)
        .collect();
    if !workloads.is_empty() {
        return Err(AppError::Invalid(format!(
            "org `{name}` still has workloads ({}); remove them first with `ikit --org {name} wl rm <name>`",
            workloads.join(", ")
        )));
    }
    for p in store.profiles().list(None).await? {
        if !p.secret_keys.is_empty() {
            secrets.delete(&format!("org/{name}/profile/{}/{}", p.kind, p.name))?;
        }
    }
    for login in store.registry().logins().await? {
        secrets.delete(&format!("org/{name}/registry/{}", login.host))?;
    }
    drop(store);
    fs::remove_dir_all(paths.org(name).dir)?;

    let mut ctx = Context::load(paths)?;
    ctx.workloads.remove(name);
    if ctx.org.as_deref() == Some(name) {
        ctx.org = None;
    }
    ctx.save(paths)
}

/// The org whose prefix matches `workload`'s, if any.
pub fn find_org_for_workload(paths: &Paths, workload: &str) -> Result<Option<String>> {
    let Ok(parsed) = workload.parse::<WorkloadName>() else {
        return Ok(None);
    };
    for name in org_names(paths)? {
        if org_config(paths, &name)?.prefix == parsed.prefix {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ikit_core::{Connection, ProfileKind};

    use super::*;
    use crate::{AddProfile, App, MemorySecretStore};

    fn new_org(name: &str) -> NewOrg {
        NewOrg {
            name: name.to_owned(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn orgs_are_isolated_and_named_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let secrets = Arc::new(MemorySecretStore::default());

        let acme = org_create(&paths, new_org("acme")).await.unwrap();
        assert!(acme.active, "first org becomes active");
        let gx = NewOrg {
            prefix: Some("gx".into()),
            envs: Some(vec!["dev".into(), "uat".into()]),
            ..new_org("globex")
        };
        let globex = org_create(&paths, gx).await.unwrap();
        assert!(!globex.active);
        assert!(paths.org("globex").state_db().exists());

        for (org, target) in [("acme", "vm"), ("globex", "vm")] {
            let app = App::open(&paths, org, secrets.clone()).await.unwrap();
            app.target_attach(target, Connection::Local).await.unwrap();
            app.workload_create("dev", target).await.unwrap();
        }
        let acme_app = App::open(&paths, "acme", secrets.clone()).await.unwrap();
        let globex_app = App::open(&paths, "globex", secrets.clone()).await.unwrap();
        assert_eq!(
            acme_app.workload_list(None).await.unwrap()[0].name,
            "wl-acme-dev-001"
        );
        assert_eq!(
            globex_app.workload_list(None).await.unwrap()[0].name,
            "gx-dev-001"
        );
        assert!(
            globex_app.workload_create("qa", "vm").await.is_err(),
            "globex has no qa env"
        );

        assert_eq!(
            find_org_for_workload(&paths, "gx-dev-001")
                .unwrap()
                .as_deref(),
            Some("globex")
        );
        assert_eq!(
            find_org_for_workload(&paths, "wl-acme-dev-001")
                .unwrap()
                .as_deref(),
            Some("acme")
        );
        assert_eq!(find_org_for_workload(&paths, "zz-dev-001").unwrap(), None);

        let listed = org_list(&paths).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|o| (o.name.as_str(), o.workloads))
                .collect::<Vec<_>>(),
            [("acme", 1), ("globex", 1)]
        );
    }

    #[tokio::test]
    async fn create_validates_and_rejects_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        org_create(&paths, new_org("acme")).await.unwrap();
        assert!(matches!(
            org_create(&paths, new_org("acme")).await,
            Err(AppError::AlreadyExists(_))
        ));
        assert!(matches!(
            org_create(&paths, new_org("Bad")).await,
            Err(AppError::Invalid(_))
        ));
        let same_prefix = NewOrg {
            prefix: Some("wl-acme".into()),
            ..new_org("acme2")
        };
        assert!(matches!(
            org_create(&paths, same_prefix).await,
            Err(AppError::AlreadyExists(_))
        ));
        assert!(!paths.org("acme2").dir.exists(), "nothing left behind");
    }

    #[tokio::test]
    async fn remove_refuses_with_workloads_and_cleans_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let secrets = Arc::new(MemorySecretStore::default());
        org_create(&paths, new_org("acme")).await.unwrap();
        let app = App::open(&paths, "acme", secrets.clone()).await.unwrap();
        let profile = AddProfile {
            kind: ProfileKind::Zerotier,
            name: "zt".into(),
            values: [("api_token".to_owned(), "t".to_owned())].into(),
        };
        app.profile_add(profile).await.unwrap();
        app.target_attach("vm", Connection::Local).await.unwrap();
        app.workload_create("dev", "vm").await.unwrap();

        assert!(matches!(
            org_remove(&paths, "acme", secrets.as_ref()).await,
            Err(AppError::Invalid(_))
        ));
        app.workload_remove("wl-acme-dev-001").await.unwrap();
        drop(app);
        org_remove(&paths, "acme", secrets.as_ref()).await.unwrap();
        assert!(!paths.org("acme").dir.exists());
        assert_eq!(secrets.get("org/acme/profile/zerotier/zt").unwrap(), None);
        assert_eq!(Context::load(&paths).unwrap().org, None);
    }
}
