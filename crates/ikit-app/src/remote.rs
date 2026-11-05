//! Remote state: one shared copy of an org in object storage, so several people can work
//! on it. The truth is a series of immutable snapshot bundles plus `HEAD.json` naming the
//! current one; the local org folder is a cache of it.
//!
//! A command that may change state takes `lock.json` (a renewed lease), brings the cache up
//! to HEAD, runs, and pushes a new snapshot if anything changed. HEAD is replaced only by a
//! conditional write against the version read at the start, so even a writer whose lock was
//! broken cannot overwrite newer state. Read-only commands take no lock.
//!
//! ```text
//! <prefix>/HEAD.json
//! <prefix>/lock.json
//! <prefix>/snapshots/000042.tar.zst
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use ikit_core::{Head, LockInfo, ProfileKind, RemoteSpec, StorageOptions, StorageProvider};
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutMode, PutOptions, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::hooks_sql::checksum_bytes;
use crate::orgs::{org_config, org_names};
use crate::paths::OrgPaths;
use crate::{App, AppError, Paths, Result};

/// How long a lock lasts without renewal; a crashed writer's lock lapses after this.
const LEASE: Duration = Duration::from_secs(10 * 60);
const RENEW_EVERY: Duration = Duration::from_secs(2 * 60);
const POLL_EVERY: Duration = Duration::from_secs(2);
/// Snapshots are kept while among the newest this many generations, or younger than
/// [`KEEP_DAYS`]; older ones are pruned after each push.
const KEEP_GENERATIONS: u64 = 50;
const KEEP_DAYS: i64 = 90;

const STATE_DB: &str = "state.db";
/// Org files that travel with the state database, besides it.
const SYNCED_FILES: &[&str] = &["config.yaml", "mcp.yaml", "mcp-tokens.yaml"];
/// Org folders whose `<slug><extension>` files travel too: the encrypted secrets and who
/// can read them.
const SYNCED_DIRS: &[(&str, &str)] = &[("recipients", ".txt"), ("secrets", ".age")];

/// Whether `name` (`dir/file`) is a file of a synced folder.
fn is_synced_dir_file(name: &str) -> bool {
    let Some((dir, file)) = name.split_once('/') else {
        return false;
    };
    SYNCED_DIRS.iter().any(|(d, ext)| {
        *d == dir
            && file
                .strip_suffix(ext)
                .is_some_and(ikit_core::is_slug)
    })
}

/// The synced folders' files present locally, as `dir/file`, sorted.
fn synced_dir_files(org: &OrgPaths) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for (dir, _) in SYNCED_DIRS {
        let path = org.dir.join(dir);
        if !path.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(path)? {
            let name = format!("{dir}/{}", entry?.file_name().to_string_lossy());
            if is_synced_dir_file(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    Ok(names)
}

fn remote_file(org: &OrgPaths) -> PathBuf {
    org.dir.join("remote.yaml")
}

fn sync_file(org: &OrgPaths) -> PathBuf {
    org.dir.join("sync.json")
}

/// What the local cache was last synced to (`sync.json`, local only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SyncState {
    generation: u64,
    /// Content checksum of the cache right after that sync; anything else means local changes.
    baseline: String,
    synced_at: DateTime<Utc>,
    /// The baseline was taken with the schema cookie pinned (see [`build_bundle`]); older
    /// baselines are converted on first use by [`local_changes`].
    #[serde(default)]
    pinned: bool,
}

impl SyncState {
    fn load(org: &OrgPaths) -> Result<Option<Self>> {
        let path = sync_file(org);
        if !path.exists() {
            return Ok(None);
        }
        let json = std::fs::read_to_string(&path)?;
        serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| AppError::Invalid(format!("{}: {e}", path.display())))
    }

    fn save(org: &OrgPaths, generation: u64, baseline: String) -> Result<()> {
        let state = Self {
            generation,
            baseline,
            synced_at: Utc::now(),
            pinned: true,
        };
        let json = serde_json::to_string_pretty(&state).expect("sync state serializes");
        std::fs::write(sync_file(org), json)?;
        Ok(())
    }
}

/// The org's remote, if it has one (`remote.yaml`).
pub fn remote_spec(paths: &Paths, org: &str) -> Result<Option<RemoteSpec>> {
    let path = remote_file(&paths.org(org));
    if !path.exists() {
        return Ok(None);
    }
    let yaml = std::fs::read_to_string(&path)?;
    serde_yaml_ng::from_str(&yaml)
        .map(Some)
        .map_err(|source| AppError::Config { path, source })
}

fn save_spec(org: &OrgPaths, spec: &RemoteSpec) -> Result<()> {
    let yaml = serde_yaml_ng::to_string(spec).expect("remote spec serializes");
    std::fs::write(remote_file(org), yaml)?;
    Ok(())
}

fn remote_error(e: object_store::Error) -> AppError {
    AppError::Invalid(format!("remote state: {e}"))
}

/// Who is acting, for lock and HEAD records: `user@host`.
pub(crate) fn whoami() -> (String, String) {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let host = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into());
    (format!("{user}@{host}"), host)
}

fn random_id() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("system randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An org's remote location: HEAD, lock and snapshots under one prefix.
#[derive(Clone)]
pub struct Remote {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    describe: String,
}

impl Remote {
    pub fn open(spec: &RemoteSpec) -> Result<Self> {
        let options = StorageOptions {
            endpoint: spec.endpoint.clone(),
            region: spec.region.clone(),
            account: spec.account.clone(),
            root: spec.root.clone(),
        };
        let get = |k: &str| match k {
            "aws_profile" => spec.aws_profile.clone(),
            "credentials_file" => spec
                .credentials_file
                .as_ref()
                .map(|p| p.display().to_string()),
            _ => None,
        };
        let store = crate::storage::build_object_store(
            "remote",
            spec.provider,
            &spec.bucket,
            &options,
            &get,
        )?;
        Ok(Self::with_store(store, &spec.prefix, spec.describe()))
    }

    pub fn with_store(store: Arc<dyn ObjectStore>, prefix: &str, describe: String) -> Self {
        Self {
            store,
            prefix: prefix.trim_matches('/').to_owned(),
            describe,
        }
    }

    pub fn describe(&self) -> &str {
        &self.describe
    }

    fn key(&self, rel: &str) -> ObjectPath {
        if self.prefix.is_empty() {
            ObjectPath::from(rel)
        } else {
            ObjectPath::from(format!("{}/{rel}", self.prefix))
        }
    }

    /// The current HEAD and the version to swap it against.
    pub async fn head(&self) -> Result<Option<(Head, UpdateVersion)>> {
        match self.get_json::<Head>("HEAD.json").await? {
            Some((head, version)) => {
                if head.format != Head::FORMAT {
                    return Err(AppError::Invalid(format!(
                        "{}: HEAD.json format {} is not supported by this ikit; upgrade ikit",
                        self.describe, head.format
                    )));
                }
                Ok(Some((head, version)))
            }
            None => Ok(None),
        }
    }

    /// Who holds the lock, if anyone.
    pub async fn lock_info(&self) -> Result<Option<LockInfo>> {
        Ok(self.get_json("lock.json").await?.map(|(l, _)| l))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        rel: &str,
    ) -> Result<Option<(T, UpdateVersion)>> {
        let got = match self.store.get(&self.key(rel)).await {
            Ok(got) => got,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(remote_error(e)),
        };
        let version = UpdateVersion {
            e_tag: got.meta.e_tag.clone(),
            version: got.meta.version.clone(),
        };
        let bytes = got.bytes().await.map_err(remote_error)?;
        let value = serde_json::from_slice(&bytes).map_err(|e| {
            AppError::Invalid(format!("{}: {rel} is not valid: {e}", self.describe))
        })?;
        Ok(Some((value, version)))
    }

    /// Writes `rel` only if it is still at `expected` (`None` = only if absent). Stores
    /// without conditional updates (the local folder) compare and write under the lock.
    async fn put_if(
        &self,
        rel: &str,
        bytes: Vec<u8>,
        expected: Option<&UpdateVersion>,
    ) -> Result<std::result::Result<UpdateVersion, ()>> {
        let path = self.key(rel);
        let mode = match expected {
            None => PutMode::Create,
            Some(v) => PutMode::Update(v.clone()),
        };
        let payload = PutPayload::from(bytes.clone());
        match self
            .store
            .put_opts(&path, payload, PutOptions::from(mode))
            .await
        {
            Ok(r) => Ok(Ok(r.into())),
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => Ok(Err(())),
            Err(object_store::Error::NotImplemented) if expected.is_some() => {
                let current = match self.store.head(&path).await {
                    Ok(meta) => meta.e_tag,
                    Err(object_store::Error::NotFound { .. }) => None,
                    Err(e) => return Err(remote_error(e)),
                };
                if current != expected.and_then(|v| v.e_tag.clone()) {
                    return Ok(Err(()));
                }
                let r = self
                    .store
                    .put(&path, PutPayload::from(bytes))
                    .await
                    .map_err(remote_error)?;
                Ok(Ok(r.into()))
            }
            Err(e) => Err(remote_error(e)),
        }
    }

    /// Takes the lock, waiting up to `timeout` for another holder to finish.
    pub async fn lock(&self, command: &str, timeout: Duration) -> Result<Lease> {
        let started = tokio::time::Instant::now();
        loop {
            let (holder, host) = whoami();
            let now = Utc::now();
            let info = LockInfo {
                id: random_id(),
                holder,
                host,
                pid: std::process::id(),
                command: command.to_owned(),
                acquired_at: now,
                expires_at: now + LEASE,
            };
            let bytes = serde_json::to_vec_pretty(&info).expect("lock serializes");
            if let Ok(version) = self.put_if("lock.json", bytes, None).await? {
                return Ok(Lease::start(self.clone(), info, version));
            }
            let Some(held) = self.lock_info().await? else {
                continue; // released between our write and read
            };
            if held.expired(Utc::now()) {
                return Err(AppError::Invalid(format!(
                    "{}: the lock held by {} ({}) expired at {}; if that command is no longer \
                     running, clear it with `ikit org unlock --force`",
                    self.describe, held.holder, held.command, held.expires_at
                )));
            }
            if started.elapsed() >= timeout {
                return Err(AppError::Invalid(format!(
                    "{} is locked by {} since {} ({}); try again later or raise --lock-timeout",
                    self.describe, held.holder, held.acquired_at, held.command
                )));
            }
            tokio::time::sleep(POLL_EVERY.min(timeout.saturating_sub(started.elapsed()))).await;
        }
    }

    /// Deletes the lock whoever holds it. Returns what was removed.
    pub async fn force_unlock(&self) -> Result<Option<LockInfo>> {
        let held = self.lock_info().await?;
        if held.is_some() {
            self.delete("lock.json").await?;
        }
        Ok(held)
    }

    async fn delete(&self, rel: &str) -> Result<()> {
        match self.store.delete(&self.key(rel)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(remote_error(e)),
        }
    }

    /// The snapshot HEAD names, checked against its checksum.
    async fn download(&self, head: &Head) -> Result<Vec<u8>> {
        let got = self
            .store
            .get(&self.key(&head.snapshot))
            .await
            .map_err(remote_error)?;
        let bytes = got.bytes().await.map_err(remote_error)?.to_vec();
        if checksum_bytes(&bytes) != head.sha256 {
            return Err(AppError::Invalid(format!(
                "{}: snapshot {} does not match the checksum in HEAD.json; nothing was changed",
                self.describe, head.snapshot
            )));
        }
        Ok(bytes)
    }

    /// Uploads `bundle` as generation `head.generation` and points HEAD at it, if HEAD is
    /// still at `base` (`None` = only if there is no HEAD yet).
    async fn commit(
        &self,
        base: Option<&UpdateVersion>,
        head: &Head,
        compressed: Vec<u8>,
    ) -> Result<()> {
        if self
            .put_if(&head.snapshot, compressed, None)
            .await?
            .is_err()
        {
            return Err(AppError::Invalid(format!(
                "{}: {} already exists; someone else wrote generation {} \
                 (run `ikit org pull` and redo the change)",
                self.describe, head.snapshot, head.generation
            )));
        }
        let json = serde_json::to_vec_pretty(head).expect("head serializes");
        if self.put_if("HEAD.json", json.clone(), base).await?.is_err() {
            return Err(AppError::Invalid(format!(
                "{}: the remote state changed while you were writing; your change was not \
                 pushed (see `ikit org status`)",
                self.describe
            )));
        }
        // The record only serves `ikit org history`; the generation is committed already.
        if let Err(e) = self
            .put_if(&Head::history_key(head.generation), json, None)
            .await
        {
            tracing::warn!("recording generation {} in history: {e}", head.generation);
        }
        Ok(())
    }

    async fn get_bytes(&self, rel: &str) -> Result<Vec<u8>> {
        let got = self.store.get(&self.key(rel)).await.map_err(remote_error)?;
        Ok(got.bytes().await.map_err(remote_error)?.to_vec())
    }

    /// Every snapshot object: (generation, key relative to the prefix, last modified).
    async fn snapshots(&self) -> Result<Vec<(u64, String, DateTime<Utc>)>> {
        use futures::TryStreamExt;
        let base = self.key("snapshots");
        let objects: Vec<object_store::ObjectMeta> = self
            .store
            .list(Some(&base))
            .try_collect()
            .await
            .map_err(remote_error)?;
        let strip = format!("{}/", self.key(""));
        let mut out: Vec<_> = objects
            .into_iter()
            .filter_map(|m| {
                let key = m.location.to_string();
                let rel = key.strip_prefix(&strip).unwrap_or(&key).to_owned();
                Head::snapshot_generation(&rel).map(|g| (g, rel, m.last_modified))
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// Generations newest first, from the history records, with snapshots written before
    /// records existed filled in from the snapshot listing.
    pub async fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let (head, _) = self.head().await?.ok_or_else(|| no_head(self))?;
        let mut by_gen: BTreeMap<u64, HistoryEntry> = BTreeMap::new();
        for (generation, snapshot, at) in self.snapshots().await? {
            if generation > head.generation {
                continue; // left by a writer that never updated HEAD
            }
            by_gen.entry(generation).or_insert(HistoryEntry {
                generation,
                written_by: None,
                written_at: at,
                command: None,
                snapshot,
                current: false,
            });
        }
        let wanted: Vec<u64> = by_gen.keys().rev().take(limit).copied().collect();
        for generation in wanted.iter().copied() {
            if generation == head.generation {
                by_gen.insert(generation, HistoryEntry::from_head(&head, true));
            } else if let Some((h, _)) = self
                .get_json::<Head>(&Head::history_key(generation))
                .await?
            {
                by_gen.insert(generation, HistoryEntry::from_head(&h, false));
            }
        }
        Ok(wanted
            .into_iter()
            .filter_map(|g| by_gen.remove(&g))
            .collect())
    }

    /// Deletes snapshots that are neither among the newest `keep` generations nor written
    /// after `cutoff`, never HEAD's. Returns how many snapshots were deleted.
    async fn prune(&self, head: &Head, keep: u64, cutoff: DateTime<Utc>) -> Result<usize> {
        let snapshots = self.snapshots().await?;
        let oldest_kept_gen = head.generation.saturating_sub(keep.saturating_sub(1));
        let mut deleted = 0;
        for (generation, key, at) in snapshots {
            if key == head.snapshot || generation >= oldest_kept_gen || at >= cutoff {
                continue;
            }
            self.delete(&key).await?;
            self.delete(&Head::history_key(generation)).await?;
            deleted += 1;
        }
        Ok(deleted)
    }
}

/// A held lock, renewed in the background until released.
pub struct Lease {
    remote: Remote,
    info: Arc<Mutex<LockInfo>>,
    lost: Arc<AtomicBool>,
    renew: tokio::task::JoinHandle<()>,
}

impl Lease {
    fn start(remote: Remote, info: LockInfo, version: UpdateVersion) -> Self {
        let info = Arc::new(Mutex::new(info));
        let lost = Arc::new(AtomicBool::new(false));
        let renew = tokio::spawn({
            let (remote, info, lost) = (remote.clone(), info.clone(), lost.clone());
            async move {
                let mut version = version;
                loop {
                    tokio::time::sleep(RENEW_EVERY).await;
                    let renewed = {
                        let mut info = info.lock().expect("lock info");
                        info.expires_at = Utc::now() + LEASE;
                        info.clone()
                    };
                    let bytes = serde_json::to_vec_pretty(&renewed).expect("lock serializes");
                    match remote.put_if("lock.json", bytes, Some(&version)).await {
                        Ok(Ok(v)) => version = v,
                        Ok(Err(())) => {
                            tracing::warn!("the remote state lock was taken over");
                            lost.store(true, Ordering::SeqCst);
                            return;
                        }
                        Err(e) => tracing::warn!("renewing the remote state lock: {e}"),
                    }
                }
            }
        });
        Self {
            remote,
            info,
            lost,
            renew,
        }
    }

    /// Stops renewing and deletes the lock if it is still ours.
    pub async fn release(self) -> Result<()> {
        self.renew.abort();
        let id = self.info.lock().expect("lock info").id.clone();
        if let Some(held) = self.remote.lock_info().await?
            && held.id == id
        {
            self.remote.delete("lock.json").await?;
        }
        Ok(())
    }

    fn lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }
}

/// The org's synced files as one uncompressed tar with fixed metadata, so equal contents
/// give equal bytes. The database is copied with `VACUUM INTO` for a consistent snapshot.
async fn build_bundle(org: &OrgPaths) -> Result<Vec<u8>> {
    bundle(org, true).await
}

async fn bundle(org: &OrgPaths, pin_cookie: bool) -> Result<Vec<u8>> {
    let tmp = tempfile::Builder::new()
        .prefix(".bundle-")
        .tempdir_in(&org.dir)?;
    let db = tmp.path().join(STATE_DB);
    ikit_state::snapshot_to(&org.state_db(), &db).await?;
    let mut tar = tar::Builder::new(Vec::new());
    let mut add = |name: &str, bytes: &[u8]| -> Result<()> {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes)?;
        Ok(())
    };
    let mut db_bytes = std::fs::read(&db)?;
    // `VACUUM INTO` writes the source's schema cookie plus one, so a database that came from
    // a snapshot would differ by that counter from the one it was made from. Pin it: the copy
    // is a standalone file no connection has open, and equal contents then give equal bytes.
    if let Some(cookie) = db_bytes.get_mut(40..44).filter(|_| pin_cookie) {
        cookie.copy_from_slice(&1u32.to_be_bytes());
    }
    add(STATE_DB, &db_bytes)?;
    for name in SYNCED_FILES {
        let path = org.dir.join(name);
        if path.exists() {
            add(name, &std::fs::read(&path)?)?;
        }
    }
    for name in synced_dir_files(org)? {
        add(&name, &std::fs::read(org.dir.join(&name))?)?;
    }
    Ok(tar.into_inner()?)
}

/// Content checksum of the local cache.
async fn fingerprint(org: &OrgPaths) -> Result<String> {
    Ok(checksum_bytes(&build_bundle(org).await?))
}

/// Whether the cache differs from what it was last synced to. A baseline from before the
/// schema cookie was pinned is re-taken when the cache still matches it the old way.
async fn local_changes(org: &OrgPaths, state: &SyncState) -> Result<bool> {
    let current = fingerprint(org).await?;
    if current == state.baseline {
        return Ok(false);
    }
    if !state.pinned && checksum_bytes(&bundle(org, false).await?) == state.baseline {
        SyncState::save(org, state.generation, current)?;
        return Ok(false);
    }
    Ok(true)
}

fn compress(tar: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::encode_all(tar, 3)?)
}

/// Replaces the org's synced files with the bundle's. Entries other than the known files
/// are refused, so a bundle cannot write outside the org folder.
fn extract_bundle(org: &OrgPaths, compressed: &[u8]) -> Result<()> {
    let tar = zstd::decode_all(compressed)?;
    std::fs::create_dir_all(&org.dir)?;
    let tmp = tempfile::Builder::new()
        .prefix(".pull-")
        .tempdir_in(&org.dir)?;
    let mut found = Vec::new();
    let mut archive = tar::Archive::new(tar.as_slice());
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if name != STATE_DB
            && !SYNCED_FILES.contains(&name.as_str())
            && !is_synced_dir_file(&name)
        {
            return Err(AppError::Invalid(format!(
                "remote snapshot holds an unexpected file `{name}`; nothing was changed"
            )));
        }
        let path = tmp.path().join(&name);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::File::create(path)?;
        std::io::copy(&mut entry, &mut file)?;
        found.push(name);
    }
    if !found.iter().any(|n| n == STATE_DB) {
        return Err(AppError::Invalid(
            "remote snapshot has no state.db; nothing was changed".to_owned(),
        ));
    }
    // A stale write-ahead log would be replayed onto the new database.
    for suffix in ["-wal", "-shm"] {
        let side = org.dir.join(format!("{STATE_DB}{suffix}"));
        if side.exists() {
            std::fs::remove_file(side)?;
        }
    }
    for name in std::iter::once(&STATE_DB).chain(SYNCED_FILES) {
        let target = org.dir.join(name);
        if found.iter().any(|n| n == name) {
            std::fs::rename(tmp.path().join(name), &target)?;
        } else if target.exists() {
            std::fs::remove_file(target)?;
        }
    }
    for name in synced_dir_files(org)? {
        if !found.contains(&name) {
            std::fs::remove_file(org.dir.join(&name))?;
        }
    }
    for name in found.iter().filter(|n| is_synced_dir_file(n)) {
        let target = org.dir.join(name);
        std::fs::create_dir_all(target.parent().expect("dir/file"))?;
        std::fs::rename(tmp.path().join(name), target)?;
    }
    Ok(())
}

/// Applies this build's migrations to the cache. Returns whether any ran.
async fn migrate(org: &OrgPaths) -> Result<bool> {
    let (_, report) = ikit_state::SqliteStore::open(&org.state_db()).await?;
    Ok(!report.applied.is_empty())
}

fn check_schema(remote: &Remote, head: &Head) -> Result<()> {
    if head.schema_version > ikit_state::schema_version() {
        return Err(AppError::Invalid(format!(
            "{} was written by ikit {} with a newer state schema ({} > {}); upgrade ikit",
            remote.describe,
            head.ikit_version,
            head.schema_version,
            ikit_state::schema_version()
        )));
    }
    Ok(())
}

/// Downloads HEAD's snapshot into the cache and records it as the new baseline.
async fn pull_into(remote: &Remote, org: &OrgPaths, head: &Head) -> Result<()> {
    check_schema(remote, head)?;
    let compressed = remote.download(head).await?;
    extract_bundle(org, &compressed)?;
    migrate(org).await?;
    SyncState::save(org, head.generation, fingerprint(org).await?)
}

/// [`pull_into`], describing changes to who can read the org's secrets.
async fn pull_noticing(remote: &Remote, org: &OrgPaths, head: &Head) -> Result<Vec<String>> {
    let before = crate::age_store::all_recipients(org);
    pull_into(remote, org, head).await?;
    let after = crate::age_store::all_recipients(org);
    let changes = crate::age_store::recipient_changes(&before, &after);
    if before.is_empty() || changes.is_empty() {
        return Ok(vec![]);
    }
    Ok(changes
        .into_iter()
        .map(|c| {
            format!(
                "{c} (generation {}, written by {})",
                head.generation, head.written_by
            )
        })
        .collect())
}

fn new_head(org: &str, generation: u64, compressed: &[u8], content: String, command: &str) -> Head {
    Head {
        format: Head::FORMAT,
        org: org.to_owned(),
        generation,
        snapshot: Head::snapshot_key(generation, &random_id()[..8]),
        sha256: checksum_bytes(compressed),
        content_sha256: content,
        schema_version: ikit_state::schema_version(),
        ikit_version: env!("CARGO_PKG_VERSION").to_owned(),
        written_by: whoami().0,
        written_at: Utc::now(),
        command: command.to_owned(),
    }
}

fn no_head(remote: &Remote) -> AppError {
    AppError::Invalid(format!(
        "{} has no HEAD.json; it may have been deleted (see `ikit org status`)",
        remote.describe
    ))
}

/// Settings for one command run against an org with a remote.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// The command line, recorded in the lock and HEAD.
    pub command: String,
    pub lock_timeout: Duration,
}

/// What [`Session::finish`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Nothing changed locally.
    Unchanged,
    /// A new generation was pushed.
    Pushed { generation: u64 },
    /// A read-only command changed local state; it stays local until the next write.
    Unpushed,
}

/// One command's hold on an org's remote: the cache is at HEAD when it starts, and
/// [`finish`](Self::finish) pushes what the command changed.
pub struct Session {
    remote: Remote,
    org: OrgPaths,
    head: Head,
    head_version: UpdateVersion,
    baseline: String,
    lease: Option<Lease>,
    command: String,
    notices: Vec<String>,
}

impl Session {
    /// Things the person should know about what the pull brought in (e.g. who can now read
    /// the org's secrets).
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Brings `org`'s cache up to its remote HEAD, taking the lock first when `write`.
    /// `None` when the org has no remote.
    pub async fn begin(
        paths: &Paths,
        org: &str,
        write: bool,
        opts: &SessionOptions,
    ) -> Result<Option<Self>> {
        let Some(spec) = remote_spec(paths, org)? else {
            return Ok(None);
        };
        let remote = Remote::open(&spec)?;
        Self::begin_with(remote, paths.org(org), write, opts)
            .await
            .map(Some)
    }

    pub async fn begin_with(
        remote: Remote,
        org: OrgPaths,
        write: bool,
        opts: &SessionOptions,
    ) -> Result<Self> {
        let lease = if write {
            Some(remote.lock(&opts.command, opts.lock_timeout).await?)
        } else {
            None
        };
        match Self::sync(&remote, &org).await {
            Ok((head, head_version, baseline, notices)) => Ok(Self {
                remote,
                org,
                head,
                head_version,
                baseline,
                lease,
                command: opts.command.clone(),
                notices,
            }),
            Err(e) => {
                if let Some(lease) = lease {
                    let _ = lease.release().await;
                }
                Err(e)
            }
        }
    }

    /// Pulls if the cache is behind and clean; returns HEAD and the cache's baseline.
    async fn sync(
        remote: &Remote,
        org: &OrgPaths,
    ) -> Result<(Head, UpdateVersion, String, Vec<String>)> {
        let (head, version) = remote.head().await?.ok_or_else(|| no_head(remote))?;
        if head.org != org.name {
            return Err(AppError::Invalid(format!(
                "{} holds org `{}`, not `{}`",
                remote.describe, head.org, org.name
            )));
        }
        check_schema(remote, &head)?;
        let local = SyncState::load(org)?;
        let have_db = org.state_db().exists();
        let dirty = match &local {
            Some(s) if have_db => local_changes(org, s).await?,
            None => have_db,
            Some(_) => false,
        };
        let local_gen = local.as_ref().map(|s| s.generation).unwrap_or(0);

        let mut notices = Vec::new();
        if head.generation != local_gen || !have_db {
            if dirty {
                return Err(AppError::Invalid(format!(
                    "local changes to org `{}` were never pushed, and {} is now at generation \
                     {} (local: {local_gen}); see `ikit org status`, then `ikit org pull \
                     --discard` to drop them",
                    org.name, remote.describe, head.generation
                )));
            }
            notices = pull_noticing(remote, org, &head).await?;
        } else if migrate(org).await? && !dirty {
            // A newer ikit migrated the cache: that is not a change of anyone's.
            SyncState::save(org, local_gen, fingerprint(org).await?)?;
        }
        let baseline = SyncState::load(org)?
            .map(|s| s.baseline)
            .unwrap_or_default();
        Ok((head, version, baseline, notices))
    }

    /// Pushes a new generation if the command changed anything, then releases the lock.
    /// Call it whether or not the command succeeded: a failed deploy still records its run.
    pub async fn finish(self) -> Result<SyncOutcome> {
        let result = self.push().await;
        if let Some(lease) = self.lease {
            let released = lease.release().await;
            if result.is_ok() {
                released?;
            }
        }
        result
    }

    async fn push(&self) -> Result<SyncOutcome> {
        let tar = build_bundle(&self.org).await?;
        let content = checksum_bytes(&tar);
        if content == self.baseline {
            return Ok(SyncOutcome::Unchanged);
        }
        let Some(lease) = &self.lease else {
            return Ok(SyncOutcome::Unpushed);
        };
        if lease.lost() {
            return Err(AppError::Invalid(format!(
                "the lock on {} was taken over while this command ran; its changes were not \
                 pushed (see `ikit org status`)",
                self.remote.describe
            )));
        }
        let compressed = compress(&tar)?;
        let generation = self.head.generation + 1;
        let head = new_head(
            &self.org.name,
            generation,
            &compressed,
            content.clone(),
            &self.command,
        );
        self.remote
            .commit(Some(&self.head_version), &head, compressed)
            .await?;
        SyncState::save(&self.org, generation, content)?;
        let cutoff = Utc::now() - chrono::Duration::days(KEEP_DAYS);
        if let Err(e) = self.remote.prune(&head, KEEP_GENERATIONS, cutoff).await {
            tracing::warn!("pruning old snapshots: {e}");
        }
        Ok(SyncOutcome::Pushed { generation })
    }
}

/// One generation in `ikit org history`. Generations written before history records existed
/// only have their snapshot's time.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub generation: u64,
    pub written_by: Option<String>,
    pub written_at: DateTime<Utc>,
    pub command: Option<String>,
    pub snapshot: String,
    /// HEAD names it.
    pub current: bool,
}

impl HistoryEntry {
    fn from_head(head: &Head, current: bool) -> Self {
        Self {
            generation: head.generation,
            written_by: Some(head.written_by.clone()),
            written_at: head.written_at,
            command: Some(head.command.clone()),
            snapshot: head.snapshot.clone(),
            current,
        }
    }
}

/// What `ikit org restore` put in the local copy (pushed as a new generation by the
/// command's session).
#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    pub org: String,
    pub restored: u64,
    pub secrets_restored: bool,
}

/// Where `ikit org status` stands.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteStatus {
    pub org: String,
    pub remote: String,
    pub local_generation: Option<u64>,
    pub synced_at: Option<DateTime<Utc>>,
    /// The cache differs from what it was last synced to.
    pub local_changes: bool,
    pub head: Option<Head>,
    pub lock: Option<LockInfo>,
}

/// What `ikit org pull` did.
#[derive(Debug, Clone, Serialize)]
pub struct PullReport {
    pub org: String,
    pub from: Option<u64>,
    pub to: u64,
    pub discarded_local_changes: bool,
    /// Changes to who can read the org's secrets.
    pub notices: Vec<String>,
}

/// The remote settings `remote set` derives from a storage and its profile.
#[derive(Debug, Clone, Default)]
pub struct RemoteOverrides {
    /// Use this named profile from ~/.aws/credentials instead of the storage profile's.
    pub aws_profile: Option<String>,
    pub region: Option<String>,
}

impl App {
    /// Makes this org remote: its state goes to `<storage prefix>/state/<org>` in `storage`,
    /// as generation 1. Refused if that path already holds state.
    /// Unless the org already has one, its keychain secrets are first moved into an
    /// encrypted file readable by `identity`, so they travel with the state.
    pub async fn remote_set(
        &self,
        storage: &str,
        overrides: RemoteOverrides,
        identity: &crate::IdentityOptions,
        command: &str,
    ) -> Result<(RemoteSpec, Head, Option<crate::SecretsInitReport>)> {
        if remote_spec(&self.paths, &self.org.name)?.is_some() {
            return Err(AppError::AlreadyExists(format!(
                "a remote for org `{}` (remove it with `ikit org remote rm`)",
                self.org.name
            )));
        }
        let spec = self.spec_from_storage(storage, overrides).await?;
        let remote = Remote::open(&spec)?;
        if let Some((head, _)) = remote.head().await? {
            return Err(AppError::AlreadyExists(format!(
                "remote state at {} (org `{}`, generation {}); use `ikit org clone` to work on it",
                remote.describe, head.org, head.generation
            )));
        }
        let secrets = if crate::org_uses_secrets_file(&self.paths, &self.org.name) {
            None
        } else {
            Some(self.secrets_init(identity, vec![]).await?)
        };
        let head = push_initial(&remote, &self.org, command).await?;
        save_spec(&self.org, &spec)?;
        Ok((spec, head, secrets))
    }

    async fn spec_from_storage(
        &self,
        name: &str,
        overrides: RemoteOverrides,
    ) -> Result<RemoteSpec> {
        let storage = self.storage_show(name).await?;
        let prefix = [storage.prefix.as_str(), "state", &self.org.name]
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("/");
        let mut spec = RemoteSpec::new(storage.provider, &storage.bucket, &prefix);
        spec.endpoint = storage.options.endpoint.clone();
        spec.region = storage.options.region.clone();
        spec.account = storage.options.account.clone();
        spec.root = storage.options.root.clone();

        // The remote must be reachable without this org's keychain secrets.
        if let Some(profile) = &storage.profile {
            let kind = match storage.provider {
                StorageProvider::S3 => ProfileKind::Aws,
                StorageProvider::Azure => ProfileKind::Azure,
                StorageProvider::Gcs => ProfileKind::Gcp,
                StorageProvider::Local => unreachable!("local storage takes no profile"),
            };
            let p = self.profile_show(kind, profile).await?;
            let has_keys = !p.secret_keys.is_empty();
            match storage.provider {
                StorageProvider::S3 => {
                    spec.aws_profile = p.config.get("aws_profile").cloned();
                    spec.region = spec.region.or_else(|| p.config.get("region").cloned());
                    if overrides.aws_profile.is_none() && (has_keys || spec.aws_profile.is_none()) {
                        return Err(AppError::Invalid(format!(
                            "profile aws/{profile} keeps its keys in the keychain, which a \
                             teammate cannot read; use a profile with `aws_profile` set, or pass \
                             --aws-profile <name from ~/.aws/credentials>"
                        )));
                    }
                }
                StorageProvider::Gcs => {
                    spec.credentials_file = p.config.get("credentials_file").map(PathBuf::from);
                }
                StorageProvider::Azure if has_keys => {
                    return Err(AppError::Invalid(format!(
                        "profile azure/{profile} keeps its client secret in the keychain; use \
                         a storage without a profile, so credentials come from the environment"
                    )));
                }
                _ => {}
            }
        }
        if let Some(p) = overrides.aws_profile {
            spec.aws_profile = Some(p);
        }
        if let Some(r) = overrides.region {
            spec.region = Some(r);
        }
        Ok(spec)
    }
}

/// Uploads the org as generation 1 to an empty remote.
async fn push_initial(remote: &Remote, org: &OrgPaths, command: &str) -> Result<Head> {
    if let Some((head, _)) = remote.head().await? {
        return Err(AppError::AlreadyExists(format!(
            "remote state at {} (org `{}`, generation {}); use `ikit org clone` to work on it",
            remote.describe, head.org, head.generation
        )));
    }
    let lease = remote.lock(command, Duration::ZERO).await?;
    let result = async {
        let tar = build_bundle(org).await?;
        let content = checksum_bytes(&tar);
        let compressed = compress(&tar)?;
        let head = new_head(&org.name, 1, &compressed, content.clone(), command);
        remote.commit(None, &head, compressed).await?;
        SyncState::save(org, 1, content)?;
        Ok::<_, AppError>(head)
    }
    .await;
    let released = lease.release().await;
    let head = result?;
    released?;
    Ok(head)
}

/// Detaches the org from its remote; the local copy becomes a local-only org and the
/// remote is left as it is.
pub fn remote_rm(paths: &Paths, org: &str) -> Result<RemoteSpec> {
    let org_paths = paths.org(org);
    let spec = remote_spec(paths, org)?
        .ok_or_else(|| AppError::NotFound(format!("a remote for org `{org}`")))?;
    std::fs::remove_file(remote_file(&org_paths))?;
    let sync = sync_file(&org_paths);
    if sync.exists() {
        std::fs::remove_file(sync)?;
    }
    Ok(spec)
}

fn require_spec(paths: &Paths, org: &str) -> Result<RemoteSpec> {
    remote_spec(paths, org)?.ok_or_else(|| {
        AppError::Invalid(format!(
            "org `{org}` has no remote; add one with `ikit org remote set --storage <name>`"
        ))
    })
}

/// Creates a local org from remote state. The org's name comes from the remote's HEAD.
pub async fn org_clone(paths: &Paths, spec: RemoteSpec) -> Result<(String, Head)> {
    let remote = Remote::open(&spec)?;
    let (head, _) = remote.head().await?.ok_or_else(|| {
        AppError::NotFound(format!(
            "remote state at {} (no HEAD.json)",
            spec.describe()
        ))
    })?;
    let name = head.org.clone();
    if !ikit_core::is_slug(&name) {
        return Err(AppError::Invalid(format!(
            "remote names an invalid org `{name}`"
        )));
    }
    let org = paths.org(&name);
    if org.dir.exists() {
        return Err(AppError::AlreadyExists(format!(
            "org `{name}` in {}",
            paths.home.display()
        )));
    }
    let cloned = async {
        pull_into(&remote, &org, &head).await?;
        let prefix = org_config(paths, &name)?.prefix;
        for other in org_names(paths)? {
            if other != name && org_config(paths, &other)?.prefix == prefix {
                return Err(AppError::AlreadyExists(format!(
                    "workload prefix `{prefix}` (used by local org `{other}`)"
                )));
            }
        }
        save_spec(&org, &spec)
    }
    .await;
    if let Err(e) = cloned {
        let _ = std::fs::remove_dir_all(&org.dir);
        return Err(e);
    }
    let mut ctx = Context::load(paths)?;
    if ctx.org.is_none() {
        ctx.org = Some(name.clone());
        ctx.save(paths)?;
    }
    Ok((name, head))
}

/// Brings the cache up to HEAD. Local changes that were never pushed are refused unless
/// `discard`.
pub async fn org_pull(paths: &Paths, org: &str, discard: bool) -> Result<PullReport> {
    let spec = require_spec(paths, org)?;
    let remote = Remote::open(&spec)?;
    let org_paths = paths.org(org);
    let (head, _) = remote.head().await?.ok_or_else(|| no_head(&remote))?;
    let local = SyncState::load(&org_paths)?;
    let dirty = match &local {
        Some(s) if org_paths.state_db().exists() => local_changes(&org_paths, s).await?,
        _ => false,
    };
    if dirty && !discard {
        return Err(AppError::Invalid(format!(
            "org `{org}` has local changes that were never pushed; pass --discard to replace \
             them with generation {}",
            head.generation
        )));
    }
    let from = local.map(|s| s.generation);
    let mut notices = vec![];
    if dirty || from != Some(head.generation) {
        notices = pull_noticing(&remote, &org_paths, &head).await?;
    }
    Ok(PullReport {
        org: org.to_owned(),
        from,
        to: head.generation,
        discarded_local_changes: dirty,
        notices,
    })
}

pub async fn org_status(paths: &Paths, org: &str) -> Result<RemoteStatus> {
    let spec = require_spec(paths, org)?;
    let remote = Remote::open(&spec)?;
    let org_paths = paths.org(org);
    let local = SyncState::load(&org_paths)?;
    let local_changes = match &local {
        Some(s) if org_paths.state_db().exists() => local_changes(&org_paths, s).await?,
        _ => false,
    };
    Ok(RemoteStatus {
        org: org.to_owned(),
        remote: remote.describe.clone(),
        local_generation: local.as_ref().map(|s| s.generation),
        synced_at: local.map(|s| s.synced_at),
        local_changes,
        head: remote.head().await?.map(|(h, _)| h),
        lock: remote.lock_info().await?,
    })
}

/// The current lock holder, for confirming before [`org_unlock`].
pub async fn org_lock_info(paths: &Paths, org: &str) -> Result<Option<LockInfo>> {
    Remote::open(&require_spec(paths, org)?)?.lock_info().await
}

/// Removes the lock whoever holds it.
pub async fn org_unlock(paths: &Paths, org: &str) -> Result<Option<LockInfo>> {
    Remote::open(&require_spec(paths, org)?)?
        .force_unlock()
        .await
}

pub async fn org_history(paths: &Paths, org: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
    Remote::open(&require_spec(paths, org)?)?.history(limit).await
}

/// Replaces the local copy with generation `generation`'s snapshot; run inside a write
/// session, whose push then records it as a new generation. The current secrets and their
/// recipients are kept unless `with_secrets`: an old snapshot may still let revoked keys in.
pub async fn org_restore(
    paths: &Paths,
    org: &str,
    generation: u64,
    with_secrets: bool,
) -> Result<RestoreReport> {
    let remote = Remote::open(&require_spec(paths, org)?)?;
    restore_into(&remote, &paths.org(org), generation, with_secrets).await
}

async fn restore_into(
    remote: &Remote,
    org: &OrgPaths,
    generation: u64,
    with_secrets: bool,
) -> Result<RestoreReport> {
    let entry = remote
        .history(usize::MAX)
        .await?
        .into_iter()
        .find(|e| e.generation == generation)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "generation {generation} of org `{}` (see `ikit org history`)",
                org.name
            ))
        })?;
    let compressed = remote.get_bytes(&entry.snapshot).await?;
    if let Some((h, _)) = remote
        .get_json::<Head>(&Head::history_key(generation))
        .await?
    {
        if checksum_bytes(&compressed) != h.sha256 {
            return Err(AppError::Invalid(format!(
                "snapshot of generation {generation} does not match its recorded checksum; \
                 nothing was changed"
            )));
        }
        check_schema(remote, &h)?;
    }
    let kept = if with_secrets {
        None
    } else {
        Some(
            synced_dir_files(org)?
                .into_iter()
                .map(|n| Ok((n.clone(), std::fs::read(org.dir.join(&n))?)))
                .collect::<Result<Vec<_>>>()?,
        )
    };
    extract_bundle(org, &compressed)?;
    if let Some(kept) = kept {
        for name in synced_dir_files(org)? {
            std::fs::remove_file(org.dir.join(name))?;
        }
        for (name, bytes) in kept {
            let path = org.dir.join(&name);
            std::fs::create_dir_all(path.parent().expect("dir/file"))?;
            std::fs::write(path, bytes)?;
        }
    }
    migrate(org).await?;
    Ok(RestoreReport {
        org: org.name.clone(),
        restored: generation,
        secrets_restored: with_secrets,
    })
}

#[cfg(test)]
pub(crate) fn test_remote() -> Remote {
    Remote::with_store(
        Arc::new(object_store::memory::InMemory::new()),
        "state/acme",
        "mem://state/acme".into(),
    )
}

#[cfg(test)]
pub(crate) mod tests;
