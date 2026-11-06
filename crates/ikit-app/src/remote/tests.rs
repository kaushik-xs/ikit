use std::sync::Arc;
use std::time::Duration;

use ikit_core::{RemoteSpec, StorageOptions, StorageProvider};

use chrono::Utc;
use ikit_core::Head;

use super::*;
use crate::testing::app;
use crate::{MemorySecretStore, NewStorageRequest};

/// A fresh identity given directly, so tests never read the real keychain: (options, public key).
pub(crate) fn test_identity() -> (crate::IdentityOptions, String) {
    let (secret, public) = crate::identity_generate();
    (
        crate::IdentityOptions {
            secret: Some(secret),
            ..Default::default()
        },
        public,
    )
}

fn opts(command: &str) -> SessionOptions {
    SessionOptions {
        command: command.into(),
        lock_timeout: Duration::ZERO,
    }
}

/// Adds a local storage named `name`: an easy way to change the org's state.
async fn change(paths: &Paths, name: &str) {
    let app = App::open(paths, "acme", Arc::new(MemorySecretStore::default()))
        .await
        .unwrap();
    app.storage_add(NewStorageRequest {
        name: name.into(),
        provider: StorageProvider::Local,
        bucket: "local".into(),
        prefix: String::new(),
        profile: None,
        options: StorageOptions {
            root: Some(paths.home.join("blobs")),
            ..Default::default()
        },
    })
    .await
    .unwrap();
}

async fn storages(paths: &Paths) -> Vec<String> {
    let app = App::open(paths, "acme", Arc::new(MemorySecretStore::default()))
        .await
        .unwrap();
    app.storage_list()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect()
}

/// A home with org `acme` pushed to `remote` as generation 1.
async fn seeded(remote: &Remote) -> (tempfile::TempDir, Paths) {
    let (dir, app, _) = app().await;
    let paths = app.paths().clone();
    drop(app);
    push_initial(remote, &paths.org("acme"), "remote set")
        .await
        .unwrap();
    (dir, paths)
}

/// A second home whose `acme` is a copy of the remote at HEAD.
async fn second_home(remote: &Remote) -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path());
    let org = paths.org("acme");
    let (head, _) = remote.head().await.unwrap().unwrap();
    pull_into(remote, &org, &head).await.unwrap();
    (dir, paths)
}

#[tokio::test]
async fn writes_push_a_generation_only_when_state_changed() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;

    let s = Session::begin_with(remote.clone(), paths.org("acme"), true, &opts("noop"))
        .await
        .unwrap();
    assert_eq!(s.finish().await.unwrap(), SyncOutcome::Unchanged);
    assert!(remote.lock_info().await.unwrap().is_none(), "lock released");

    let s = Session::begin_with(
        remote.clone(),
        paths.org("acme"),
        true,
        &opts("storage add"),
    )
    .await
    .unwrap();
    change(&paths, "nas").await;
    assert_eq!(
        s.finish().await.unwrap(),
        SyncOutcome::Pushed { generation: 2 }
    );
    let (head, _) = remote.head().await.unwrap().unwrap();
    assert_eq!(head.generation, 2);
    assert_eq!(head.command, "storage add");
    assert_eq!(head.org, "acme");
    assert!(remote.lock_info().await.unwrap().is_none());
}

#[tokio::test]
async fn another_home_picks_up_changes_on_its_next_command() {
    let remote = test_remote();
    let (_a, alice) = seeded(&remote).await;
    let (_b, bob) = second_home(&remote).await;

    let s = Session::begin_with(remote.clone(), alice.org("acme"), true, &opts("add"))
        .await
        .unwrap();
    change(&alice, "nas").await;
    s.finish().await.unwrap();

    assert!(storages(&bob).await.is_empty());
    let s = Session::begin_with(
        remote.clone(),
        bob.org("acme"),
        false,
        &opts("storage list"),
    )
    .await
    .unwrap();
    assert_eq!(storages(&bob).await, ["nas"]);
    assert_eq!(s.finish().await.unwrap(), SyncOutcome::Unchanged);
}

#[tokio::test]
async fn a_second_writer_waits_then_fails_naming_the_holder() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;
    let held = Session::begin_with(remote.clone(), paths.org("acme"), true, &opts("apply"))
        .await
        .unwrap();
    let err = Session::begin_with(remote.clone(), paths.org("acme"), true, &opts("other"))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("is locked by") && err.contains("apply"),
        "{err}"
    );
    held.finish().await.unwrap();
    Session::begin_with(remote.clone(), paths.org("acme"), true, &opts("other"))
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();
}

#[tokio::test]
async fn a_writer_whose_lock_was_broken_cannot_overwrite_newer_state() {
    let remote = test_remote();
    let (_a, alice) = seeded(&remote).await;
    let (_b, bob) = second_home(&remote).await;

    let slow = Session::begin_with(remote.clone(), alice.org("acme"), true, &opts("slow"))
        .await
        .unwrap();
    change(&alice, "from-alice").await;

    remote.force_unlock().await.unwrap();
    let fast = Session::begin_with(remote.clone(), bob.org("acme"), true, &opts("fast"))
        .await
        .unwrap();
    change(&bob, "from-bob").await;
    assert_eq!(
        fast.finish().await.unwrap(),
        SyncOutcome::Pushed { generation: 2 }
    );

    let err = slow.finish().await.unwrap_err().to_string();
    assert!(
        err.contains("already exists") || err.contains("changed"),
        "{err}"
    );
    let (head, _) = remote.head().await.unwrap().unwrap();
    assert_eq!(head.command, "fast");

    // Alice's unpushed change blocks pulling over it until she discards it.
    let err = Session::begin_with(remote.clone(), alice.org("acme"), false, &opts("list"))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("never pushed"), "{err}");
}

#[tokio::test]
async fn a_read_command_that_changes_state_does_not_push() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;
    let s = Session::begin_with(remote.clone(), paths.org("acme"), false, &opts("list"))
        .await
        .unwrap();
    change(&paths, "nas").await;
    assert_eq!(s.finish().await.unwrap(), SyncOutcome::Unpushed);
    assert_eq!(remote.head().await.unwrap().unwrap().0.generation, 1);

    // The next write pushes it.
    let s = Session::begin_with(remote.clone(), paths.org("acme"), true, &opts("write"))
        .await
        .unwrap();
    assert_eq!(
        s.finish().await.unwrap(),
        SyncOutcome::Pushed { generation: 2 }
    );
}

#[tokio::test]
async fn a_tampered_snapshot_is_refused() {
    let remote = test_remote();
    let (_dir, _paths) = seeded(&remote).await;
    let (head, _) = remote.head().await.unwrap().unwrap();
    remote
        .store
        .put(
            &remote.key(&head.snapshot),
            PutPayload::from(b"junk".to_vec()),
        )
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = pull_into(&remote, &Paths::new(dir.path()).org("acme"), &head)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not match the checksum"), "{err}");
}

#[tokio::test]
async fn a_newer_schema_is_refused() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;
    let (mut head, version) = remote.head().await.unwrap().unwrap();
    head.schema_version += 1;
    let json = serde_json::to_vec(&head).unwrap();
    remote
        .put_if("HEAD.json", json, Some(&version))
        .await
        .unwrap()
        .unwrap();
    let err = Session::begin_with(remote, paths.org("acme"), false, &opts("list"))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("upgrade ikit"), "{err}");
}

#[tokio::test]
async fn bundles_with_unexpected_files_are_refused() {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(1);
    header.set_cksum();
    tar.append_data(&mut header, "keys/id_rsa", &b"x"[..])
        .unwrap();
    let compressed = compress(&tar.into_inner().unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = extract_bundle(&Paths::new(dir.path()).org("acme"), &compressed)
        .unwrap_err()
        .to_string();
    assert!(err.contains("unexpected file"), "{err}");
}

#[tokio::test]
async fn equal_state_gives_equal_bundles() {
    let (_dir, app, _) = app().await;
    let org = app.org().clone();
    assert_eq!(
        fingerprint(&org).await.unwrap(),
        fingerprint(&org).await.unwrap()
    );
}

/// The whole flow over a local-folder remote, which has no conditional updates.
#[tokio::test]
async fn remote_set_clone_pull_and_status_over_a_folder() {
    let (_a, app, _) = app().await;
    let alice = app.paths().clone();
    let shared = tempfile::tempdir().unwrap();
    app.storage_add(NewStorageRequest {
        name: "nas".into(),
        provider: StorageProvider::Local,
        bucket: "local".into(),
        prefix: "ikit".into(),
        profile: None,
        options: StorageOptions {
            root: Some(shared.path().to_path_buf()),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    let (spec, head, secrets) = app
        .remote_set(
            "nas",
            RemoteOverrides::default(),
            &test_identity().0,
            "org remote set",
        )
        .await
        .unwrap();
    assert!(
        secrets.is_some(),
        "keychain secrets moved to the encrypted file"
    );
    drop(app);
    assert_eq!(spec.prefix, "ikit/state/acme");
    assert_eq!(head.generation, 1);
    assert!(shared.path().join("ikit/state/acme/HEAD.json").exists());

    let b = tempfile::tempdir().unwrap();
    let bob = Paths::new(b.path());
    let url = format!("local:{}/ikit/state/acme", shared.path().display());
    let (name, _) = org_clone(&bob, RemoteSpec::parse_url(&url).unwrap())
        .await
        .unwrap();
    assert_eq!(name, "acme");
    assert_eq!(storages(&bob).await, ["nas"]);
    for f in ["recipients/org.txt", "secrets/org.age"] {
        assert!(
            bob.org("acme").dir.join(f).exists(),
            "{f} travels with the state"
        );
    }
    assert!(
        org_clone(&bob, RemoteSpec::parse_url(&url).unwrap())
            .await
            .is_err(),
        "already cloned"
    );

    let s = Session::begin(&alice, "acme", true, &opts("add"))
        .await
        .unwrap()
        .unwrap();
    change(&alice, "second").await;
    assert_eq!(
        s.finish().await.unwrap(),
        SyncOutcome::Pushed { generation: 2 }
    );

    let status = org_status(&bob, "acme").await.unwrap();
    assert_eq!(status.local_generation, Some(1));
    assert_eq!(status.head.unwrap().generation, 2);
    assert!(!status.local_changes);

    let report = org_pull(&bob, "acme", false).await.unwrap();
    assert_eq!((report.from, report.to), (Some(1), 2));
    assert_eq!(storages(&bob).await, ["nas", "second"]);

    change(&bob, "local-only").await;
    assert!(org_status(&bob, "acme").await.unwrap().local_changes);
    assert!(org_pull(&bob, "acme", false).await.is_err());
    let report = org_pull(&bob, "acme", true).await.unwrap();
    assert!(report.discarded_local_changes);
    assert_eq!(storages(&bob).await, ["nas", "second"]);

    remote_rm(&bob, "acme").unwrap();
    assert!(remote_spec(&bob, "acme").unwrap().is_none());
}

#[tokio::test]
async fn remote_set_refuses_keychain_credentials() {
    let (_dir, app, _) = app().await;
    app.profile_add(crate::AddProfile {
        kind: ikit_core::ProfileKind::Aws,
        name: "keys".into(),
        values: [
            ("region".to_owned(), "eu-west-1".to_owned()),
            ("access_key_id".to_owned(), "AKIA".to_owned()),
            ("secret_access_key".to_owned(), "s".to_owned()),
        ]
        .into(),
    })
    .await
    .unwrap();
    app.storage_add(NewStorageRequest {
        name: "s3".into(),
        provider: StorageProvider::S3,
        bucket: "b".into(),
        prefix: String::new(),
        profile: Some("keys".into()),
        options: StorageOptions::default(),
    })
    .await
    .unwrap();
    let err = app
        .remote_set("s3", RemoteOverrides::default(), &test_identity().0, "x")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("keychain"), "{err}");
}

/// Pushes one change from `paths` as a write session named `command`.
async fn write(remote: &Remote, paths: &Paths, storage: &str, command: &str) -> u64 {
    let s = Session::begin_with(remote.clone(), paths.org("acme"), true, &opts(command))
        .await
        .unwrap();
    change(paths, storage).await;
    match s.finish().await.unwrap() {
        SyncOutcome::Pushed { generation } => generation,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn history_lists_generations_newest_first() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;
    write(&remote, &paths, "a", "add a").await;
    write(&remote, &paths, "b", "add b").await;
    let history = remote.history(10).await.unwrap();
    let gens: Vec<u64> = history.iter().map(|h| h.generation).collect();
    assert_eq!(gens, [3, 2, 1]);
    assert!(history[0].current);
    assert_eq!(history[0].command.as_deref(), Some("add b"));
    assert_eq!(history[2].command.as_deref(), Some("remote set"));
    assert_eq!(remote.history(1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn restore_brings_back_an_old_generation_as_a_new_one() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;
    write(&remote, &paths, "a", "add a").await;
    write(&remote, &paths, "b", "add b").await;
    assert_eq!(storages(&paths).await, ["a", "b"]);

    let s = Session::begin_with(remote.clone(), paths.org("acme"), true, &opts("restore 2"))
        .await
        .unwrap();
    restore_into(&remote, &paths.org("acme"), 2, false)
        .await
        .unwrap();
    assert_eq!(
        s.finish().await.unwrap(),
        SyncOutcome::Pushed { generation: 4 }
    );
    assert_eq!(storages(&paths).await, ["a"]);
    assert!(
        restore_into(&remote, &paths.org("acme"), 99, false)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn restore_keeps_the_current_secrets_unless_asked() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;
    let org = paths.org("acme");
    let recipients = org.dir.join("recipients/org.txt");
    std::fs::create_dir_all(recipients.parent().unwrap()).unwrap();
    std::fs::write(&recipients, "age1old # old\n").unwrap();
    let s = Session::begin_with(remote.clone(), org.clone(), true, &opts("old keys"))
        .await
        .unwrap();
    s.finish().await.unwrap();
    std::fs::write(&recipients, "age1new # new\n").unwrap();
    let s = Session::begin_with(remote.clone(), org.clone(), true, &opts("new keys"))
        .await
        .unwrap();
    s.finish().await.unwrap();

    restore_into(&remote, &org, 2, false).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&recipients).unwrap(),
        "age1new # new\n"
    );
    restore_into(&remote, &org, 2, true).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&recipients).unwrap(),
        "age1old # old\n"
    );
}

#[tokio::test]
async fn a_snapshot_left_by_a_crashed_writer_does_not_block_the_next() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;
    // A writer uploaded generation 2's snapshot, then died before updating HEAD.
    remote
        .put_if(&Head::snapshot_key(2, "dead"), b"partial".to_vec(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(write(&remote, &paths, "a", "after crash").await, 2);
    let history = remote.history(10).await.unwrap();
    assert_eq!(history[0].command.as_deref(), Some("after crash"));
}

#[tokio::test]
async fn prune_keeps_head_recent_and_newest_generations() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;
    for i in 0..4 {
        write(&remote, &paths, &format!("s{i}"), "w").await;
    }
    let (head, _) = remote.head().await.unwrap().unwrap();
    assert_eq!(head.generation, 5);
    // Everything is recent: nothing goes.
    let past = Utc::now() - chrono::Duration::days(1);
    assert_eq!(remote.prune(&head, 2, past).await.unwrap(), 0);
    // With a cutoff in the future only the newest two generations stay.
    let future = Utc::now() + chrono::Duration::days(1);
    assert_eq!(remote.prune(&head, 2, future).await.unwrap(), 3);
    let gens: Vec<u64> = remote
        .history(10)
        .await
        .unwrap()
        .iter()
        .map(|h| h.generation)
        .collect();
    assert_eq!(gens, [5, 4]);
}

#[tokio::test]
async fn pulling_reports_who_gained_or_lost_access_to_secrets() {
    let remote = test_remote();
    let (_a, alice) = seeded(&remote).await;
    let org = alice.org("acme");
    let (_, a) = crate::age_store::LocalIdentity::generate();
    let (_, b) = crate::age_store::LocalIdentity::generate();
    let recipients = org.dir.join("recipients/org.txt");
    std::fs::create_dir_all(recipients.parent().unwrap()).unwrap();
    std::fs::write(&recipients, format!("{} # alice\n", a.public)).unwrap();
    Session::begin_with(remote.clone(), org.clone(), true, &opts("init"))
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();
    let (_b, bob) = second_home(&remote).await;

    std::fs::write(&recipients, format!("{} # bob\n", b.public)).unwrap();
    Session::begin_with(remote.clone(), org, true, &opts("swap"))
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();

    let s = Session::begin_with(remote.clone(), bob.org("acme"), false, &opts("list"))
        .await
        .unwrap();
    let notices = s.notices().join("\n");
    assert!(
        notices.contains("bob (") && notices.contains("can now read org"),
        "{notices}"
    );
    assert!(
        notices.contains("alice (") && notices.contains("can no longer read"),
        "{notices}"
    );
    s.finish().await.unwrap();
}

#[tokio::test]
async fn copies_at_the_same_generation_fingerprint_equal() {
    let remote = test_remote();
    let (_a, alice) = seeded(&remote).await;
    let (_b, bob) = second_home(&remote).await;
    assert_eq!(
        fingerprint(&alice.org("acme")).await.unwrap(),
        fingerprint(&bob.org("acme")).await.unwrap()
    );
}

#[tokio::test]
async fn a_baseline_from_before_cookie_pinning_is_converted_not_reported_as_changes() {
    let remote = test_remote();
    let (_dir, paths) = seeded(&remote).await;
    let org = paths.org("acme");
    // What an older ikit recorded: the unpinned checksum, no `pinned` field.
    let old = checksum_bytes(&bundle(&org, false).await.unwrap());
    std::fs::write(
        sync_file(&org),
        format!(r#"{{"generation":1,"baseline":"{old}","synced_at":"2026-11-05T00:00:00Z"}}"#),
    )
    .unwrap();
    let s = Session::begin_with(remote.clone(), org.clone(), false, &opts("list"))
        .await
        .unwrap();
    assert_eq!(s.finish().await.unwrap(), SyncOutcome::Unchanged);
    let state = SyncState::load(&org).unwrap().unwrap();
    assert!(state.pinned);
    assert!(!local_changes(&org, &state).await.unwrap());
}
