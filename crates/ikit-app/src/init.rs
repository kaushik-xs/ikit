use std::fs;
use std::path::PathBuf;

use ikit_state::StateStore;
use serde::Serialize;

use crate::context::Context;
use crate::orgs::{open_and_seed, org_config, write_org_config};
use crate::{AppError, Paths, Result, SecretStore};

/// The org created from a home that predates orgs.
pub const LEGACY_ORG: &str = "default";

/// Marks that the `default` org still has keychain entries under their pre-org names.
/// Removed once they are renamed, so a failed rename is retried by the next `ikit init`.
const PENDING_SECRETS_MARKER: &str = ".legacy-secrets-pending";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitReport {
    pub home: PathBuf,
    /// Set when a pre-org home was moved into the `default` org.
    pub migrated_legacy: Option<LegacyMigration>,
    /// Keychain entries renamed into the `default` org's namespace by this run.
    pub legacy_secrets_renamed: Option<usize>,
    pub orgs: Vec<OrgInit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyMigration {
    pub org: String,
    pub moved: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrgInit {
    pub name: String,
    pub migrations_applied: Vec<i64>,
    pub backup: Option<PathBuf>,
    /// Built-in registry services added by this run (`name@version`).
    pub seeded: Vec<String>,
}

/// Creates the ikit home, moves a pre-org home into the `default` org, then migrates and
/// seeds every org. Safe to re-run.
pub async fn init(paths: &Paths, secrets: &dyn SecretStore) -> Result<InitReport> {
    fs::create_dir_all(paths.orgs_dir())?;
    let migrated_legacy = if paths.legacy_state_db().exists() {
        Some(migrate_legacy(paths).await?)
    } else {
        None
    };
    let marker = paths.org(LEGACY_ORG).dir.join(PENDING_SECRETS_MARKER);
    let legacy_secrets_renamed = if marker.exists() {
        let renamed = rename_legacy_secrets(paths, secrets).await.map_err(|e| {
            AppError::Invalid(format!(
                "the state is in org `{LEGACY_ORG}`, but renaming its keychain entries failed: {e}\n\
                 Allow ikit to access the keychain if macOS asks, then re-run `ikit init` to retry."
            ))
        })?;
        fs::remove_file(&marker)?;
        Some(renamed)
    } else {
        None
    };

    let mut orgs = Vec::new();
    for org in crate::org_list(paths).await? {
        // Surface a broken config now rather than on the first command that needs it.
        org_config(paths, &org.name)?;
        let (_, report, seeded) = open_and_seed(paths, &org.name).await?;
        orgs.push(OrgInit {
            name: org.name,
            migrations_applied: report.applied,
            backup: report.backup,
            seeded,
        });
    }
    Ok(InitReport {
        home: paths.home.clone(),
        migrated_legacy,
        legacy_secrets_renamed,
        orgs,
    })
}

/// Moves `state.db`, `config.yaml` and `workloads/` from the home into `orgs/default/` and
/// rewrites the context. Workload names keep the `wl` prefix, so nothing is renamed.
/// Keychain entries are renamed afterwards by [`rename_legacy_secrets`].
async fn migrate_legacy(paths: &Paths) -> Result<LegacyMigration> {
    let org = paths.org(LEGACY_ORG);
    if org.dir.exists() {
        return Err(AppError::Invalid(format!(
            "both {} and {} exist; move one of them aside and re-run `ikit init`",
            paths.legacy_state_db().display(),
            org.dir.display()
        )));
    }
    fs::create_dir_all(&org.dir)?;
    fs::write(org.dir.join(PENDING_SECRETS_MARKER), "")?;

    let mut moved = Vec::new();
    for entry in fs::read_dir(&paths.home)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let is_state = name == "state.db"
            || name.starts_with("state.db-")
            || name.starts_with("state.db.bak-");
        if is_state || name == "config.yaml" || name == "workloads" {
            fs::rename(paths.home.join(&name), org.dir.join(&name))?;
            moved.push(name);
        }
    }
    moved.sort();
    // Write the config back with its prefix (`wl`) spelled out.
    let config = org_config(paths, LEGACY_ORG)?;
    write_org_config(paths, LEGACY_ORG, &config)?;

    let old = Context::load(paths)?;
    let mut ctx = Context {
        org: Some(LEGACY_ORG.to_owned()),
        ..Context::default()
    };
    if let Some(wl) = old.workload {
        ctx.workloads.insert(LEGACY_ORG.to_owned(), wl);
    }
    ctx.save(paths)?;

    tracing::info!(org = LEGACY_ORG, ?moved, "moved pre-org home into an org");
    Ok(LegacyMigration {
        org: LEGACY_ORG.to_owned(),
        moved,
    })
}

/// Renames keychain entries from their pre-org names into the `default` org's namespace.
/// Idempotent: entries already renamed are skipped. Returns how many were renamed.
async fn rename_legacy_secrets(paths: &Paths, secrets: &dyn SecretStore) -> Result<usize> {
    let (store, _, _) = open_and_seed(paths, LEGACY_ORG).await?;
    let mut keys = Vec::new();
    for p in store
        .profiles()
        .list(None)
        .await?
        .into_iter()
        .filter(|p| !p.secret_keys.is_empty())
    {
        keys.push(format!("profile/{}/{}", p.kind, p.name));
    }
    for w in store.workloads().list(None).await? {
        for s in store
            .workloads()
            .services(w.id)
            .await?
            .into_iter()
            .filter(|s| !s.secret_env.is_empty())
        {
            keys.push(format!("workload/{}/{}", w.name, s.name));
        }
    }
    let mut renamed = 0;
    for key in keys {
        if let Some(value) = secrets.get(&key)? {
            secrets.set(&format!("org/{LEGACY_ORG}/{key}"), &value)?;
            secrets.delete(&key)?;
            renamed += 1;
        }
    }
    Ok(renamed)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use ikit_core::target::TargetOrigin;
    use ikit_core::{Category, Connection, ProfileKind, VersionConstraint};
    use ikit_state::{NewProfile, NewService, SqliteStore};

    use super::*;
    use crate::{App, MemorySecretStore, NewOrg, org_create};

    #[tokio::test]
    async fn init_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("home"));
        let secrets = MemorySecretStore::default();

        let first = init(&paths, &secrets).await.unwrap();
        assert!(first.orgs.is_empty() && first.migrated_legacy.is_none());
        org_create(
            &paths,
            NewOrg {
                name: "acme".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let second = init(&paths, &secrets).await.unwrap();
        assert_eq!(second.orgs.len(), 1);
        assert!(second.orgs[0].migrations_applied.is_empty() && second.orgs[0].seeded.is_empty());
    }

    #[tokio::test]
    async fn init_rejects_broken_org_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        org_create(
            &paths,
            NewOrg {
                name: "acme".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        fs::write(paths.org("acme").config_file(), "envs: 42").unwrap();
        assert!(matches!(
            init(&paths, &MemorySecretStore::default()).await,
            Err(AppError::Config { .. })
        ));
    }

    /// Builds a home in the pre-org layout (state.db, config.yaml, context.yaml and
    /// keychain entries at the top level) and checks `init` moves it into `default`.
    #[tokio::test]
    async fn migrates_a_pre_org_home_into_the_default_org() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let secrets = Arc::new(MemorySecretStore::default());
        {
            let (store, _) = SqliteStore::open(&paths.legacy_state_db()).await.unwrap();
            let seeds = crate::registry::seeds();
            store
                .registry()
                .import(
                    &seeds.services,
                    ikit_core::registry::ImportMode::Merge,
                    ikit_core::registry::Source::Seed,
                )
                .await
                .unwrap();
            let keys = vec!["api_token".to_owned()];
            let config = BTreeMap::new();
            let profile = NewProfile {
                kind: ProfileKind::Zerotier,
                name: "zt",
                config: &config,
                secret_keys: &keys,
            };
            store.profiles().insert(profile).await.unwrap();
            let target = store
                .targets()
                .insert("laptop", TargetOrigin::Attached, &Connection::Local)
                .await
                .unwrap();
            let wl = store
                .workloads()
                .create("wl", "dev", target.id)
                .await
                .unwrap();
            let pg = store.registry().get("postgres").await.unwrap().unwrap();
            let constraint: VersionConstraint = "16".parse().unwrap();
            let secret_env = vec!["POSTGRES_PASSWORD".to_owned()];
            let env = BTreeMap::new();
            let service = NewService {
                name: "postgres",
                group: Category::Data,
                registry_service_id: pg.service.id,
                version_id: pg.versions.last().unwrap().id,
                constraint: &constraint,
                env: &env,
                secret_env: &secret_env,
            };
            store
                .workloads()
                .add_services(wl.id, &[service])
                .await
                .unwrap();
        }
        fs::write(paths.home.join("config.yaml"), "envs: [dev, prod]\n").unwrap();
        fs::write(paths.home.join("context.yaml"), "workload: wl-dev-001\n").unwrap();
        fs::create_dir_all(paths.home.join("workloads")).unwrap();
        fs::write(
            paths.home.join("workloads/wl-dev-001.yaml"),
            "name: wl-dev-001\n",
        )
        .unwrap();
        secrets
            .set("profile/zerotier/zt", r#"{"api_token":"t"}"#)
            .unwrap();
        secrets
            .set(
                "workload/wl-dev-001/postgres",
                r#"{"POSTGRES_PASSWORD":"pw"}"#,
            )
            .unwrap();

        let report = init(&paths, secrets.as_ref()).await.unwrap();
        let migrated = report.migrated_legacy.unwrap();
        assert!(
            migrated.moved.contains(&"state.db".to_owned()),
            "{:?}",
            migrated.moved
        );
        assert!(
            migrated.moved.contains(&"config.yaml".to_owned())
                && migrated.moved.contains(&"workloads".to_owned())
        );
        assert_eq!(report.legacy_secrets_renamed, Some(2));
        assert!(
            !paths
                .org(LEGACY_ORG)
                .dir
                .join(PENDING_SECRETS_MARKER)
                .exists()
        );
        assert!(!paths.legacy_state_db().exists());

        let org = paths.org(LEGACY_ORG);
        assert!(org.state_db().exists() && org.workload_spec("wl-dev-001").exists());
        let config = org_config(&paths, LEGACY_ORG).unwrap();
        assert_eq!((config.prefix.as_str(), config.envs.len()), ("wl", 2));
        assert_eq!(secrets.get("profile/zerotier/zt").unwrap(), None);
        assert!(
            secrets
                .get("org/default/profile/zerotier/zt")
                .unwrap()
                .is_some()
        );
        assert!(
            secrets
                .get("org/default/workload/wl-dev-001/postgres")
                .unwrap()
                .is_some()
        );

        let app = App::open(&paths, LEGACY_ORG, secrets.clone())
            .await
            .unwrap();
        assert_eq!(
            app.current_workload().unwrap().as_deref(),
            Some("wl-dev-001")
        );
        assert_eq!(
            app.workload_create("dev", "laptop").await.unwrap().name,
            "wl-dev-002"
        );

        // Re-running finds nothing left to migrate.
        assert!(
            init(&paths, secrets.as_ref())
                .await
                .unwrap()
                .migrated_legacy
                .is_none()
        );
    }

    /// A keychain that refuses access until `allow` is set, like macOS without a prompt.
    #[derive(Default)]
    struct LockedKeychain {
        inner: MemorySecretStore,
        allow: std::sync::atomic::AtomicBool,
    }

    impl SecretStore for LockedKeychain {
        fn get(&self, key: &str) -> std::result::Result<Option<String>, crate::SecretError> {
            if !self.allow.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::SecretError::new("In dark wake, no UI possible"));
            }
            self.inner.get(key)
        }
        fn set(&self, key: &str, value: &str) -> std::result::Result<(), crate::SecretError> {
            self.inner.set(key, value)
        }
        fn delete(&self, key: &str) -> std::result::Result<(), crate::SecretError> {
            self.inner.delete(key)
        }
    }

    #[tokio::test]
    async fn failed_keychain_rename_is_retried_by_the_next_init() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        {
            let (store, _) = SqliteStore::open(&paths.legacy_state_db()).await.unwrap();
            let keys = vec!["api_token".to_owned()];
            let config = BTreeMap::new();
            let profile = NewProfile {
                kind: ProfileKind::Zerotier,
                name: "zt",
                config: &config,
                secret_keys: &keys,
            };
            store.profiles().insert(profile).await.unwrap();
        }
        let keychain = LockedKeychain::default();
        keychain.inner.set("profile/zerotier/zt", "t").unwrap();

        let err = init(&paths, &keychain).await.unwrap_err();
        assert!(err.to_string().contains("re-run `ikit init`"), "{err}");
        assert!(
            paths.org(LEGACY_ORG).state_db().exists(),
            "files were moved"
        );

        keychain
            .allow
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let report = init(&paths, &keychain).await.unwrap();
        assert_eq!(report.legacy_secrets_renamed, Some(1));
        assert_eq!(
            keychain
                .inner
                .get("org/default/profile/zerotier/zt")
                .unwrap()
                .as_deref(),
            Some("t")
        );
        assert_eq!(
            init(&paths, &keychain)
                .await
                .unwrap()
                .legacy_secrets_renamed,
            None
        );
    }

    /// Same as above against the real OS keychain, with entries written by ikit itself as a
    /// pre-org build would have. Opt-in: `cargo test -p ikit-app -- --ignored`.
    #[tokio::test]
    #[ignore = "touches the real OS keychain"]
    async fn migrates_real_keychain_entries() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let name = format!("ikit-test-{}", std::process::id());
        {
            let (store, _) = SqliteStore::open(&paths.legacy_state_db()).await.unwrap();
            let keys = vec!["api_token".to_owned()];
            let config = BTreeMap::new();
            let profile = NewProfile {
                kind: ProfileKind::Zerotier,
                name: &name,
                config: &config,
                secret_keys: &keys,
            };
            store.profiles().insert(profile).await.unwrap();
        }
        let keychain = crate::KeyringStore;
        let old = format!("profile/zerotier/{name}");
        let new = format!("org/default/{old}");
        keychain.set(&old, "secret").unwrap();

        let report = init(&paths, &keychain).await;
        let (old_after, new_after) = (keychain.get(&old), keychain.get(&new));
        keychain.delete(&old).unwrap();
        keychain.delete(&new).unwrap();

        assert_eq!(report.unwrap().legacy_secrets_renamed, Some(1));
        assert_eq!(old_after.unwrap(), None);
        assert_eq!(new_after.unwrap().as_deref(), Some("secret"));
    }
}
