//! Session-owned append-only observation journal.
//!
//! Layout under `state_dir()/observations/`:
//! - `obs-<session>.jsonl` — one serialized [`Observation`] per line.
//! - `obs-<session>.meta.json` — base revision, compaction and gap counters.
//! - `locks/obs-<session>.lock` — serializes append/compaction across
//!   processes; every file and directory is owner-only (0o600/0o700).
//!
//! The journal is bounded: appends that would exceed [`MAX_JOURNAL_BYTES`]
//! compact the oldest lines and advance `base_revision`, so revisions stay
//! scope-monotonic without ever rewinding.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use uuid::Uuid;

use crate::config;

use super::{OBSERVATION_SCHEMA_VERSION, Observation, ObservationKind};

/// Hard cap for one session journal. Compaction keeps the newest records so
/// the live context window stays available while revisions keep advancing.
pub(crate) const MAX_JOURNAL_BYTES: u64 = 8 << 20;
/// After compaction the journal is left under this size.
const COMPACTION_TARGET_BYTES: u64 = MAX_JOURNAL_BYTES - (MAX_JOURNAL_BYTES / 4);

/// Per-journal in-process state; refreshed under the file lock when the file
/// grew from an outside writer (rescan is bounded by the journal cap).
#[derive(Default)]
struct JournalCache {
    file_len: u64,
    revision: u64,
    dedupe: HashMap<String, u64>,
}

fn journal_caches() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<JournalCache>>>> {
    static CACHES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<JournalCache>>>>> = OnceLock::new();
    CACHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Durable sidecar carrying the compaction base revision and gap counters.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct JournalMeta {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    base_revision: u64,
    #[serde(default)]
    write_failures: u64,
    #[serde(default)]
    compactions: u64,
    #[serde(default)]
    last_write_error: Option<String>,
}

/// Owner-facing journal summary for `context_status` / the debug CLI.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct JournalStatus {
    pub schema_version: u32,
    pub exists: bool,
    pub revision: u64,
    pub base_revision: u64,
    pub observations: u64,
    pub bytes: u64,
    pub max_bytes: u64,
    pub compactions: u64,
    pub write_failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_write_error: Option<String>,
    pub corrupt_lines: u64,
    /// True when recorded writes failed or lines could not be parsed; the
    /// resolver surfaces this as an explicit gap instead of silent absence.
    pub degraded: bool,
}

/// Read filters for journal scans.
#[derive(Clone, Debug, Default)]
pub(crate) struct ListFilter {
    pub kind: Option<ObservationKind>,
    pub task_id: Option<String>,
    pub after_revision: Option<u64>,
    /// Cap on returned records, applied from the newest backwards; the
    /// response keeps ascending revision order.
    pub limit: Option<usize>,
}

/// Outcome of one append attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppendOutcome {
    /// The record was appended and assigned this scope-monotonic revision.
    Appended { revision: u64 },
    /// The dedupe key was already present; no record was appended.
    Duplicate { revision: u64 },
}

pub(crate) struct ObservationStore {
    directory: PathBuf,
}

struct HeldLock {
    file: fs::File,
}

impl HeldLock {
    fn new(path: &Path) -> Result<Self> {
        let file = open_private_lock_file(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            anyhow::ensure!(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
                "cannot acquire observation journal lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(Self { file })
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

impl ObservationStore {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// The default store under the owner-only state directory.
    pub(crate) fn default_store() -> Result<Self> {
        Ok(Self::new(config::state_dir()?.join("observations")))
    }

    fn journal_path(&self, session_id: &str) -> Result<PathBuf> {
        config::validate_session_id(session_id)?;
        Ok(self.directory.join(format!("obs-{session_id}.jsonl")))
    }

    fn meta_path(&self, session_id: &str) -> Result<PathBuf> {
        config::validate_session_id(session_id)?;
        Ok(self.directory.join(format!("obs-{session_id}.meta.json")))
    }

    fn lock_path(&self, session_id: &str) -> Result<PathBuf> {
        config::validate_session_id(session_id)?;
        Ok(self
            .directory
            .join("locks")
            .join(format!("obs-{session_id}.lock")))
    }

    fn prepare(&self, session_id: &str) -> Result<HeldLock> {
        ensure_private_directory(&self.directory)?;
        ensure_private_directory(&self.directory.join("locks"))?;
        HeldLock::new(&self.lock_path(session_id)?)
    }

    fn cache_for(path: &Path) -> Result<Arc<Mutex<JournalCache>>> {
        let mut caches = journal_caches()
            .lock()
            .map_err(|_| anyhow::anyhow!("observation cache lock poisoned"))?;
        Ok(caches
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(JournalCache::default())))
            .clone())
    }

    /// Read the meta sidecar; absent or unparsable content falls back to a
    /// fresh default so a corrupt sidecar cannot wedge the journal.
    fn read_meta(&self, session_id: &str) -> JournalMeta {
        let path = match self.meta_path(session_id) {
            Ok(path) => path,
            Err(_) => return JournalMeta::default(),
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => return JournalMeta::default(),
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn write_meta(&self, session_id: &str, meta: &JournalMeta) -> Result<()> {
        let path = self.meta_path(session_id)?;
        let mut value = serde_json::to_value(meta)?;
        value["schema_version"] = Value::from(OBSERVATION_SCHEMA_VERSION);
        write_private_file(&path, serde_json::to_string(&value)?.as_bytes())
    }

    /// Record a write failure in the sidecar so `context_status` can surface
    /// the gap; best-effort because the caller must never fail over journal
    /// bookkeeping.
    pub(crate) fn note_write_failure(&self, session_id: &str, error: &anyhow::Error) {
        let update = || -> Result<()> {
            let _lock = self.prepare(session_id)?;
            let mut meta = self.read_meta(session_id);
            meta.write_failures = meta.write_failures.saturating_add(1);
            let message = error.to_string();
            meta.last_write_error = Some(message.chars().take(256).collect());
            self.write_meta(session_id, &meta)
        };
        let _ = update();
    }

    /// Refresh the in-process dedupe map under the held lock when the file
    /// changed outside this cache (another writer or an external compaction).
    fn refresh_cache_locked(
        &self,
        session_id: &str,
        journal: &Path,
        cache: &mut MutexGuard<'_, JournalCache>,
    ) -> Result<u64> {
        let actual_len = match fs::metadata(journal) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_file(),
                    "observation journal is not a regular file"
                );
                reject_symlink_target(journal)?;
                metadata.len()
            }
            Err(error) if is_not_found(&error) => 0,
            Err(error) => return Err(error.into()),
        };
        if actual_len == cache.file_len {
            return Ok(actual_len);
        }
        let base = self.read_meta(session_id).base_revision;
        if actual_len == 0 {
            cache.dedupe.clear();
            cache.revision = base;
            cache.file_len = 0;
            return Ok(0);
        }
        let (records, _corrupt) = read_journal(journal)?;
        // Revisions are stored on each record, so torn or corrupt lines cannot
        // break the chain: the next append follows the last record's revision.
        cache.dedupe = records
            .iter()
            .map(|record| (record.dedupe_key.clone(), record.revision))
            .collect();
        cache.revision = records
            .last()
            .map(|record| record.revision)
            .unwrap_or(base)
            .max(base);
        cache.file_len = actual_len;
        Ok(actual_len)
    }

    /// Append one observation unless its dedupe key is already recorded.
    ///
    /// Returns `Duplicate` on replay so retried operations do not multiply
    /// journal entries; the assigned revision makes the journal a
    /// scope-monotonic cursor (`--after-revision`, `at_least_revision`).
    pub(crate) fn append(&self, mut observation: Observation) -> Result<AppendOutcome> {
        let _lock = self.prepare(&observation.session_id)?;
        let journal = self.journal_path(&observation.session_id)?;
        let cache = Self::cache_for(&journal)?;
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow::anyhow!("observation cache lock poisoned"))?;
        let current_len =
            self.refresh_cache_locked(&observation.session_id, &journal, &mut cache)?;
        if let Some(revision) = cache.dedupe.get(&observation.dedupe_key) {
            return Ok(AppendOutcome::Duplicate {
                revision: *revision,
            });
        }

        let mut meta = self.read_meta(&observation.session_id);
        let revision = cache.revision.saturating_add(1).max(meta.base_revision);
        observation.revision = revision;
        let mut line = serde_json::to_vec(&observation)?;
        line.push(b'\n');

        let new_len = current_len.saturating_add(line.len() as u64);
        if new_len > MAX_JOURNAL_BYTES {
            self.compact_locked(&observation.session_id, &journal, &mut cache, &mut meta)?;
        }
        // Open after any compaction: a handle acquired earlier would append
        // to the renamed-away inode.
        let mut file = open_private_append_file(&journal)?;
        file.write_all(&line)?;
        file.sync_all()?;
        cache
            .dedupe
            .insert(observation.dedupe_key.clone(), revision);
        cache.revision = revision;
        cache.file_len = fs::metadata(&journal).map(|m| m.len()).unwrap_or(new_len);
        Ok(AppendOutcome::Appended { revision })
    }

    /// Keep the newest records that fit the compaction target; rewrite under
    /// the held lock and advance `base_revision` so old revisions stay
    /// referable for staleness checks.
    fn compact_locked(
        &self,
        session_id: &str,
        journal: &Path,
        cache: &mut JournalCache,
        meta: &mut JournalMeta,
    ) -> Result<()> {
        let (records, _corrupt) = read_journal(journal)?;
        let mut kept: Vec<&Observation> = Vec::new();
        let mut kept_bytes = 0usize;
        for record in records.iter().rev() {
            let size = serde_json::to_vec(record)?.len() + 1;
            if kept_bytes.saturating_add(size) > COMPACTION_TARGET_BYTES as usize {
                break;
            }
            kept.push(record);
            kept_bytes += size;
        }
        kept.reverse();
        let dropped = records.len().saturating_sub(kept.len());
        let mut payload = Vec::with_capacity(kept_bytes);
        for record in &kept {
            payload.extend_from_slice(&serde_json::to_vec(record)?);
            payload.push(b'\n');
        }
        write_private_file(journal, &payload)?;
        meta.base_revision = meta.base_revision.saturating_add(dropped as u64);
        meta.compactions = meta.compactions.saturating_add(1);
        self.write_meta(session_id, meta)?;
        cache.dedupe = kept
            .iter()
            .map(|record| (record.dedupe_key.clone(), record.revision))
            .collect();
        cache.revision = kept
            .last()
            .map(|record| record.revision)
            .unwrap_or(meta.base_revision);
        cache.file_len = payload.len() as u64;
        Ok(())
    }

    /// Read journal records newest-last option: ascending revision order,
    /// tolerant of a torn final line (concurrent append or crash).
    pub(crate) fn list(
        &self,
        session_id: &str,
        filter: &ListFilter,
    ) -> Result<(Vec<Observation>, u64)> {
        let journal = self.journal_path(session_id)?;
        let records = match fs::metadata(&journal) {
            Ok(metadata) if metadata.is_file() => {
                let _lock = self.prepare(session_id)?;
                read_journal(&journal)?
            }
            _ => (Vec::new(), 0),
        };
        let (records, corrupt) = records;
        let mut filtered: Vec<Observation> = records
            .into_iter()
            .filter(|record| record.session_id == session_id)
            .filter(|record| {
                filter
                    .after_revision
                    .is_none_or(|after| record.revision > after)
            })
            .filter(|record| filter.kind.is_none_or(|kind| record.kind == kind))
            .filter(|record| {
                filter
                    .task_id
                    .as_deref()
                    .is_none_or(|task| record.task_id.as_deref() == Some(task))
            })
            .collect();
        if let Some(limit) = filter.limit
            && filtered.len() > limit
        {
            filtered = filtered.split_off(filtered.len() - limit);
        }
        Ok((filtered, corrupt))
    }

    /// Fetch one record by id.
    pub(crate) fn get(&self, session_id: &str, id: Uuid) -> Result<Option<Observation>> {
        let (records, _corrupt) = self.list(
            session_id,
            &ListFilter {
                limit: None,
                ..ListFilter::default()
            },
        )?;
        Ok(records.into_iter().find(|record| record.id == id))
    }

    /// Journal counters for `context_status` and the debug CLI.
    pub(crate) fn status(&self, session_id: &str) -> Result<JournalStatus> {
        let journal = self.journal_path(session_id)?;
        let meta = self.read_meta(session_id);
        let mut status = JournalStatus {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            max_bytes: MAX_JOURNAL_BYTES,
            base_revision: meta.base_revision,
            compactions: meta.compactions,
            write_failures: meta.write_failures,
            last_write_error: meta.last_write_error.clone(),
            ..JournalStatus::default()
        };
        match fs::metadata(&journal) {
            Ok(metadata) if metadata.is_file() => {
                let _lock = self.prepare(session_id)?;
                let (records, corrupt) = read_journal(&journal)?;
                status.exists = true;
                status.bytes = metadata.len();
                status.corrupt_lines = corrupt;
                status.observations = records
                    .iter()
                    .filter(|r| r.session_id == session_id)
                    .count() as u64;
                status.revision = records
                    .iter()
                    .map(|record| record.revision)
                    .max()
                    .unwrap_or(meta.base_revision);
            }
            Ok(_) => anyhow::bail!("observation journal is not a regular file"),
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error.into()),
        }
        status.degraded = status.write_failures > 0 || status.corrupt_lines > 0;
        Ok(status)
    }
}

/// Parse the journal tolerantly: a torn tail line (crash mid-append) counts
/// as corrupt instead of failing the whole read.
fn read_journal(path: &Path) -> Result<(Vec<Observation>, u64)> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("cannot open observation journal {}", path.display()))?;
    reject_symlink_target(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("cannot read observation journal {}", path.display()))?;
    let mut records = Vec::new();
    let mut corrupt = 0u64;
    let mut seen: HashSet<Uuid> = HashSet::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Observation>(line) {
            Ok(record)
                if record.schema_version == OBSERVATION_SCHEMA_VERSION
                    && seen.insert(record.id) =>
            {
                records.push(record)
            }
            _ => corrupt += 1,
        }
    }
    Ok((records, corrupt))
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("private observation path has no parent")?;
    reject_symlink_target(path)?;
    let temporary = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let write_result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("cannot create {}", temporary.display()))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn open_private_append_file(path: &Path) -> Result<fs::File> {
    reject_symlink_target(path)?;
    let mut options = fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
        .open(path)
        .with_context(|| format!("cannot open observation journal {}", path.display()))
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::metadata(path) {
        Ok(_) => validate_store_directory(path),
        Err(error) if is_not_found(&error) => create_private_directory(path),
        Err(error) => Err(error.into()),
    }
}

fn validate_store_directory(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("cannot stat private directory {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir(),
        "private path {} is not a directory",
        path.display()
    );
    reject_symlink_target(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "private directory {} must not be accessible by other users",
            path.display()
        );
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("cannot create private directory {}", path.display()))?;
    validate_store_directory(path)
}

fn open_private_lock_file(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .with_context(|| format!("cannot open lock file {}", path.display()))?;
    reject_symlink_target(path)?;
    Ok(file)
}

fn reject_symlink_target(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path);
    match metadata {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "refusing symlink at {}",
                path.display()
            );
            Ok(())
        }
        Err(error) if is_not_found(&error) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn is_not_found(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

#[cfg(test)]
mod tests {
    use super::super::{
        ActorRef, Observation, ObservationContent, ObservationKind, Provenance, SessionInstanceRef,
        StateRef, TargetRef,
    };
    use super::*;
    use crate::test_support;

    fn tempdir() -> PathBuf {
        test_support::private_process_root()
            .unwrap()
            .join("observation-test")
            .join(Uuid::new_v4().simple().to_string())
    }

    fn record(session_id: &str, dedupe: &str) -> Observation {
        Observation {
            id: Uuid::new_v4(),
            schema_version: OBSERVATION_SCHEMA_VERSION,
            observed_at: 1,
            accepted_at: None,
            session_id: session_id.to_owned(),
            session_instance: SessionInstanceRef {
                started_at: 0,
                process_id: 0,
            },
            repository: None,
            workspace_id: None,
            task_id: Some("task-1".to_owned()),
            execution_id: None,
            operation_id: None,
            actor: ActorRef {
                transport: "test".to_owned(),
                principal: None,
            },
            target: TargetRef {
                backend: "codex".to_owned(),
            },
            action: "task_get".to_owned(),
            kind: ObservationKind::ExecutionState,
            content: ObservationContent::View {
                view: serde_json::json!({"task_id": "task-1", "status": "running"}),
            },
            state_ref: Some(StateRef {
                task_id: Some("task-1".to_owned()),
                status: Some("running".to_owned()),
                revision: Some(1),
                generation: Some(1),
            }),
            evidence_refs: Vec::new(),
            provenance: Provenance {
                tool: "codex_task_get".to_owned(),
                source: "orchestration".to_owned(),
                control_action: None,
            },
            revision: 0,
            dedupe_key: dedupe.to_owned(),
        }
    }

    #[test]
    fn append_dedupes_and_assigns_monotonic_revisions() {
        let dir = tempdir();
        let store = ObservationStore::new(dir.clone());
        let sid = Uuid::new_v4().to_string();
        let first = store.append(record(&sid, "k1")).unwrap();
        let repeat = store.append(record(&sid, "k1")).unwrap();
        let second = store.append(record(&sid, "k2")).unwrap();
        assert_eq!(first, AppendOutcome::Appended { revision: 1 });
        assert_eq!(repeat, AppendOutcome::Duplicate { revision: 1 });
        assert_eq!(second, AppendOutcome::Appended { revision: 2 });
        let (records, corrupt) = store.list(&sid, &ListFilter::default()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(corrupt, 0);
        assert_eq!(records[0].revision, 1);
        assert_eq!(records[1].revision, 2);
        let status = store.status(&sid).unwrap();
        assert!(status.exists);
        assert_eq!(status.revision, 2);
        assert_eq!(status.observations, 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
            let journal = dir.join(format!("obs-{sid}.jsonl"));
            let mode = fs::metadata(journal).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn after_revision_and_filters_select_records() {
        let dir = tempdir();
        let store = ObservationStore::new(dir);
        let sid = Uuid::new_v4().to_string();
        store.append(record(&sid, "a")).unwrap();
        store.append(record(&sid, "b")).unwrap();
        store.append(record(&sid, "c")).unwrap();
        let (records, _) = store
            .list(
                &sid,
                &ListFilter {
                    after_revision: Some(1),
                    ..ListFilter::default()
                },
            )
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].revision, 2);
        let (limited, _) = store
            .list(
                &sid,
                &ListFilter {
                    limit: Some(1),
                    ..ListFilter::default()
                },
            )
            .unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].revision, 3);
    }

    #[test]
    fn corrupt_tail_line_is_skipped_not_fatal() {
        let dir = tempdir();
        let store = ObservationStore::new(dir.clone());
        let sid = Uuid::new_v4().to_string();
        store.append(record(&sid, "ok")).unwrap();
        let journal = dir.join(format!("obs-{sid}.jsonl"));
        let mut file = fs::OpenOptions::new().append(true).open(&journal).unwrap();
        file.write_all(b"{\"partial\":").unwrap();
        let (records, corrupt) = store.list(&sid, &ListFilter::default()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(corrupt, 1);
        assert!(store.status(&sid).unwrap().degraded);
    }

    #[test]
    fn write_failure_is_recorded_in_status() {
        let dir = tempdir();
        let store = ObservationStore::new(dir);
        let sid = Uuid::new_v4().to_string();
        store.note_write_failure(&sid, &anyhow::anyhow!("disk full"));
        let status = store.status(&sid).unwrap();
        assert_eq!(status.write_failures, 1);
        assert!(status.degraded);
        assert_eq!(status.last_write_error.as_deref(), Some("disk full"));
    }
}
