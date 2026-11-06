use std::sync::Arc;

use super::*;
use crate::Paths;

/// A fresh identity: (identity, its recipient line labelled `label`).
fn person(label: &str) -> (Arc<LocalIdentity>, RecipientEntry) {
    let (_, id) = LocalIdentity::generate();
    let entry = RecipientEntry {
        key: id.public.clone(),
        label: Some(label.into()),
    };
    (Arc::new(id), entry)
}

fn org(dir: &tempfile::TempDir) -> OrgPaths {
    Paths::new(dir.path()).org("acme")
}

fn secrets(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn round_trips_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, a) = person("alice");
    let store = AgeFileStore::new(org(&dir), Some(alice.clone()));
    store
        .init(std::slice::from_ref(&a), secrets(&[("org/acme/x", "1")]))
        .unwrap();
    assert!(uses_secrets_file(&org(&dir)));
    store.set("org/acme/y", "2").unwrap();
    store.delete("org/acme/x").unwrap();
    store.delete("org/acme/missing").unwrap();

    let again = AgeFileStore::new(org(&dir), Some(alice));
    assert_eq!(again.get("org/acme/x").unwrap(), None);
    assert_eq!(again.get("org/acme/y").unwrap().as_deref(), Some("2"));
    assert_eq!(again.keys().unwrap(), ["org/acme/y"]);

    let raw = std::fs::read(secrets_file(&org(&dir))).unwrap();
    assert!(
        !String::from_utf8_lossy(&raw).contains("org/acme/y"),
        "encrypted"
    );
}

#[test]
fn only_recipients_read_and_write() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, a) = person("alice");
    let (bob, b) = person("bob");
    AgeFileStore::new(org(&dir), Some(alice.clone()))
        .init(&[a], secrets(&[("k", "v")]))
        .unwrap();

    let as_bob = AgeFileStore::new(org(&dir), Some(bob.clone()));
    let e = as_bob.get("k").unwrap_err().to_string();
    assert!(e.contains("cannot read") && e.contains(&bob.public), "{e}");
    let none = AgeFileStore::new(org(&dir), None);
    assert!(
        none.get("k")
            .unwrap_err()
            .to_string()
            .contains("ikit identity create")
    );

    let as_alice = AgeFileStore::new(org(&dir), Some(alice.clone()));
    assert!(as_alice.grant("org", b.clone()).unwrap());
    assert!(!as_alice.grant("org", b).unwrap(), "already a recipient");
    let as_bob = AgeFileStore::new(org(&dir), Some(bob));
    assert_eq!(as_bob.get("k").unwrap().as_deref(), Some("v"));
    as_bob.set("k", "from bob").unwrap();
    let fresh = AgeFileStore::new(org(&dir), Some(alice));
    assert_eq!(fresh.get("k").unwrap().as_deref(), Some("from bob"));
}

#[test]
fn revoked_keys_cannot_read_the_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, a) = person("alice");
    let (bob, b) = person("bob");
    let as_alice = AgeFileStore::new(org(&dir), Some(alice.clone()));
    as_alice.init(&[a, b], secrets(&[("k", "v")])).unwrap();

    let removed = as_alice.revoke("org", "bob").unwrap();
    assert_eq!(removed.key, bob.public);
    let e = AgeFileStore::new(org(&dir), Some(bob))
        .get("k")
        .unwrap_err()
        .to_string();
    assert!(e.contains("cannot read"), "{e}");
    assert!(
        as_alice.revoke("org", &alice.public).is_err(),
        "the last recipient stays"
    );
    assert!(as_alice.revoke("org", "nobody").is_err());
}

#[test]
fn a_non_recipient_cannot_add_keys_or_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, a) = person("alice");
    let (mallory, m) = person("mallory");
    AgeFileStore::new(org(&dir), Some(alice))
        .init(&[a], secrets(&[]))
        .unwrap();
    let as_mallory = AgeFileStore::new(org(&dir), Some(mallory));
    assert!(as_mallory.grant("org", m).is_err());
    assert!(as_mallory.set("k", "v").is_err());
}

#[test]
fn tampered_files_fail_to_decrypt() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, a) = person("alice");
    AgeFileStore::new(org(&dir), Some(alice.clone()))
        .init(&[a], secrets(&[("k", "v")]))
        .unwrap();
    let path = secrets_file(&org(&dir));
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(AgeFileStore::new(org(&dir), Some(alice)).get("k").is_err());
}

#[test]
fn parses_recipient_lines() {
    let (_, id) = LocalIdentity::generate();
    let e = RecipientEntry::parse(&format!("{}   # kaushik@rg2k", id.public)).unwrap();
    assert_eq!(e.key, id.public);
    assert_eq!(e.label.as_deref(), Some("kaushik@rg2k"));

    let ssh = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHsKLqeplhpW+uObz5dvMgjz1OxfM/XXUB+VHtZ6isGN me@laptop";
    let e = RecipientEntry::parse(ssh).unwrap();
    assert_eq!(
        e.key,
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHsKLqeplhpW+uObz5dvMgjz1OxfM/XXUB+VHtZ6isGN"
    );
    assert_eq!(e.label, None);

    for bad in ["", "hello", "age1notakey", "ssh-ed25519"] {
        assert!(RecipientEntry::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn identities_load_from_a_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let (secret, id) = LocalIdentity::generate();
    let path = dir.path().join("key.txt");
    std::fs::write(
        &path,
        format!("# created: now\n# public key: x\n{}\n", *secret),
    )
    .unwrap();
    assert_eq!(LocalIdentity::from_file(&path).unwrap().public, id.public);
    assert!(LocalIdentity::from_secret("AGE-SECRET-KEY-nope").is_err());
}

#[tokio::test]
async fn init_moves_keychain_secrets_and_the_org_switches_stores() {
    let (_dir, app, keychain) = crate::testing::app().await;
    app.profile_add(crate::AddProfile {
        kind: ikit_core::ProfileKind::Zerotier,
        name: "zt".into(),
        values: [
            ("org_id".to_owned(), "o".to_owned()),
            ("api_token".to_owned(), "tok".to_owned()),
        ]
        .into(),
    })
    .await
    .unwrap();
    let key = "org/acme/profile/zerotier/zt";
    assert!(keychain.get(key).unwrap().is_some());

    let (identity, public) = crate::remote::tests::test_identity();
    let report = app.secrets_init(&identity, vec![]).await.unwrap();
    assert_eq!(report.moved, [key]);
    assert_eq!(report.recipients[0].key, public);
    assert!(app.secrets_init(&identity, vec![]).await.is_err(), "once");

    let paths = app.paths().clone();
    let store = crate::org_secret_store(&paths, "acme", &identity).unwrap();
    assert!(store.get(key).unwrap().unwrap().contains("tok"));

    let reopened = crate::App::open(&paths, "acme", store).await.unwrap();
    let values = reopened
        .profile_values(ikit_core::ProfileKind::Zerotier, "zt")
        .await
        .unwrap();
    assert_eq!(values.get("api_token").map(String::as_str), Some("tok"));

    let forgotten = reopened
        .secrets_forget_keychain(keychain.as_ref())
        .await
        .unwrap();
    assert_eq!(forgotten, [key]);
    assert!(keychain.get(key).unwrap().is_none());
}

const PROD: &str = "org/acme/workload/wl-acme-prod-001/db";
const DEV: &str = "org/acme/workload/wl-acme-dev-001/db";
const HOOK: &str = "org/acme/hook/wl-acme-prod-001/notify";
const PROFILE: &str = "org/acme/profile/zerotier/zt";

#[test]
fn split_gives_an_env_its_own_readers() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, a) = person("alice");
    let (bob, b) = person("bob");
    let as_alice = AgeFileStore::new(org(&dir), Some(alice.clone()));
    as_alice
        .init(
            &[a.clone(), b],
            secrets(&[(PROD, "p"), (DEV, "d"), (HOOK, "h"), (PROFILE, "z")]),
        )
        .unwrap();

    let moved = as_alice.split("prod", std::slice::from_ref(&a)).unwrap();
    assert_eq!(moved, [HOOK, PROD]);
    assert!(
        as_alice.split("prod", std::slice::from_ref(&a)).is_err(),
        "once"
    );
    assert!(as_alice.split("org", std::slice::from_ref(&a)).is_err());

    // Bob reads dev and org secrets but not prod's.
    let as_bob = AgeFileStore::new(org(&dir), Some(bob.clone()));
    assert_eq!(as_bob.get(DEV).unwrap().as_deref(), Some("d"));
    assert_eq!(as_bob.get(PROFILE).unwrap().as_deref(), Some("z"));
    let e = as_bob.get(PROD).unwrap_err().to_string();
    assert!(e.contains("prod.age") && e.contains("--env prod"), "{e}");
    assert!(
        as_bob
            .set("org/acme/workload/wl-acme-prod-002/x", "v")
            .is_err()
    );

    // New prod secrets land in the prod file.
    let fresh = AgeFileStore::new(org(&dir), Some(alice.clone()));
    fresh
        .set("org/acme/workload/wl-acme-prod-002/x", "v")
        .unwrap();
    let scopes = fresh.scopes().unwrap();
    assert_eq!(scopes[0].scope, "org");
    assert_eq!(scopes[0].secrets, Some(2));
    assert_eq!(scopes[1].scope, "prod");
    assert_eq!(scopes[1].secrets, Some(3));

    let merged = fresh.merge("prod").unwrap();
    assert_eq!(merged.len(), 3);
    assert!(!recipients_path(&org(&dir), "prod").exists());
    let as_bob = AgeFileStore::new(org(&dir), Some(bob));
    assert_eq!(as_bob.get(PROD).unwrap().as_deref(), Some("p"));
    assert!(fresh.merge("prod").is_err());
}

#[test]
fn only_org_readers_may_split_and_the_splitter_must_keep_access() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, a) = person("alice");
    let (bob, b) = person("bob");
    AgeFileStore::new(org(&dir), Some(alice.clone()))
        .init(std::slice::from_ref(&a), secrets(&[(PROD, "p")]))
        .unwrap();
    let as_bob = AgeFileStore::new(org(&dir), Some(bob));
    assert!(as_bob.split("prod", std::slice::from_ref(&b)).is_err());
    let as_alice = AgeFileStore::new(org(&dir), Some(alice));
    assert!(
        as_alice.split("prod", &[b]).is_err(),
        "alice left herself out"
    );
    assert!(
        !recipients_path(&org(&dir), "prod").exists(),
        "nothing written"
    );
}

#[test]
fn keys_map_to_their_workload_env() {
    assert_eq!(key_env(PROD).as_deref(), Some("prod"));
    assert_eq!(key_env(HOOK).as_deref(), Some("prod"));
    assert_eq!(key_env(PROFILE), None);
    assert_eq!(key_env("org/acme/registry/ghcr.io"), None);
    assert_eq!(key_env("org/acme/workload/not-a-name/x"), None);
}

#[tokio::test]
async fn check_wants_a_recovery_key_and_two_people() {
    let (_dir, app, _) = crate::testing::app().await;
    let (identity, _) = crate::remote::tests::test_identity();
    app.secrets_init(&identity, vec![]).await.unwrap();
    let paths = app.paths().clone();
    let problems = crate::secrets_check(&paths, "acme", 2).unwrap();
    assert_eq!(problems.len(), 2, "{problems:?}");

    let (_, recovery) = person("recovery");
    let (_, ci) = person("ci-prod");
    for r in [recovery, ci] {
        crate::secrets_grant(&paths, "acme", &identity, &r.key, r.label, None).unwrap();
    }
    let problems = crate::secrets_check(&paths, "acme", 2).unwrap();
    assert_eq!(problems.len(), 1, "a CI key is not a person: {problems:?}");
    let (_, bob) = person("bob");
    crate::secrets_grant(&paths, "acme", &identity, &bob.key, bob.label, None).unwrap();
    assert!(crate::secrets_check(&paths, "acme", 2).unwrap().is_empty());
}
