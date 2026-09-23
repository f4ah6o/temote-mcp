//! OpenCode `serve` task backend.
//!
//! Mirrors the Codex app-server task contract over a per-task `opencode serve`
//! child process on 127.0.0.1 (HTTP via `unofficial-opencode-sdk`): durable
//! scope-bound task records with idempotent operation receipts, runtime leases
//! that fence one serve owner per task across Temote processes, bounded scoped
//! evidence, and session-owner lifecycle fencing.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use uuid::Uuid;

use crate::{config, evidence};

const TASK_SCHEMA_VERSION: u64 = 1;
const TASK_RETENTION_SECONDS: u64 = 24 * 60 * 60;
const MAX_TASK_RECORD_BYTES: usize = 64 * 1024;
const MAX_TASK_DIRECTORY_ENTRIES: usize = 4096;
const MAX_TASKS_PER_SCOPE: usize = 128;
const MAX_OPERATION_HISTORY: usize = 32;
const MAX_ARGUMENT_BYTES: usize = 256;
const MAX_TASK_INPUT_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 1024;
const MAX_REPORT_BYTES: usize = 8 * 1024;
const MAX_REPORT_ARRAY_ITEMS: usize = 64;
const MAX_SUMMARY_CHARS: usize = 1200;
const MAX_MESSAGES_SCAN: u32 = 16;
const SERVE_HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
const SERVE_HEALTH_POLL: Duration = Duration::from_millis(150);
const SERVE_HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SERVE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const SERVE_TAIL_BYTES: usize = 8 * 1024;
const SERVE_SPAWN_ATTEMPTS: usize = 2;
const SERVE_CONTRACT_ENV: &str = "TEMOTE_OPENCODE_SERVE_CONTRACT";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ServeContract {
    Auto,
    V1,
    V2,
}

fn serve_contract_override() -> ServeContract {
    match std::env::var(SERVE_CONTRACT_ENV)
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "v1" | "1" | "global" => ServeContract::V1,
        "v2" | "2" | "api" => ServeContract::V2,
        _ => ServeContract::Auto,
    }
}
const CHILD_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const SESSION_STOP_POLL: Duration = Duration::from_secs(1);
const SESSION_TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const PROMPT_ADMISSION_GRACE_SECONDS: u64 = 30;

const TASK_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x0c, 0x9e, 0x4a, 0x7d, 0x2b, 0x51, 0x48, 0xe6, 0xa3, 0x90, 0x7c, 0x62, 0xd1, 0xf4, 0x55, 0x0b,
]);
const REQUEST_FINGERPRINT_NAMESPACE: Uuid = Uuid::from_bytes([
    0x71, 0x38, 0xb2, 0x0e, 0x9d, 0x45, 0x4f, 0x6a, 0x8c, 0x21, 0x55, 0xe7, 0x3b, 0x9a, 0xc0, 0x64,
]);
const MESSAGE_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x4f, 0x1b, 0x8d, 0x33, 0x5a, 0x6c, 0x49, 0x82, 0x91, 0x5e, 0x67, 0xd8, 0x20, 0xa4, 0x0f, 0xb3,
]);

const SERVE_CHILD_ENV_ALLOWLIST: &[&str] = &[
    "ALL_PROXY",
    "HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "LANG",
    "LOGNAME",
    "NO_PROXY",
    "PATH",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "USER",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
];

const REPORT_INSTRUCTIONS: &str = r#"You are a delegated implementation worker running non-interactively. You must not ask interactive questions, and you must finish the task below before answering.

When finished, respond with ONLY one JSON object and nothing else: no markdown, no code fences, no text before or after the JSON.
The JSON object must contain exactly these fields:
{"status":"completed|failed|blocked|needs_decision","summary":"short summary, at most 1200 characters","base_commit":"","changed_files":[],"checks":[],"unresolved":[],"requested_model":__REQUESTED_MODEL__,"requested_effort":__REQUESTED_EFFORT__,"observed_model":null,"observed_effort":null}
Rules:
- All string values are plain strings; changed_files, checks, and unresolved are arrays of strings (use [] when empty).
- Set "requested_model" to __REQUESTED_MODEL__ and "requested_effort" to __REQUESTED_EFFORT__.
- Set "observed_model"/"observed_effort" only when you can actually observe them; otherwise keep null.
- Do not include any other fields.

Task:
"#;

const RESUME_INSTRUCTIONS: &str = "Continue the task. When finished, respond with ONLY the required JSON report object and nothing else.";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
struct SessionInstance {
    id: String,
    started_at: u64,
    process_id: u32,
}

struct LifecycleEntry {
    closing: bool,
    cleanup_complete: bool,
    in_flight: usize,
    cancellation: watch::Sender<bool>,
    drain_notify: Arc<tokio::sync::Notify>,
}

impl LifecycleEntry {
    fn new() -> Self {
        let (cancellation, _) = watch::channel(false);
        Self {
            closing: false,
            cleanup_complete: false,
            in_flight: 0,
            cancellation,
            drain_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

#[derive(Default)]
struct LifecycleRegistry {
    entries: HashMap<SessionInstance, LifecycleEntry>,
}

fn lifecycle_registry() -> &'static Mutex<LifecycleRegistry> {
    static REGISTRY: OnceLock<Mutex<LifecycleRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(LifecycleRegistry::default()))
}

pub(crate) fn begin_session_shutdown(session: &config::Session) {
    begin_session_instance_shutdown(&SessionInstance::from_session(session));
}

fn begin_session_instance_shutdown(owner: &SessionInstance) {
    let mut registry = lifecycle_registry().lock().unwrap();
    let entry = registry
        .entries
        .entry(owner.clone())
        .or_insert_with(LifecycleEntry::new);
    if !entry.closing {
        entry.closing = true;
        let _ = entry.cancellation.send(true);
    }
}

fn finish_session_shutdown(owner: &SessionInstance) -> Result<()> {
    let mut registry = lifecycle_registry().lock().unwrap();
    let Some(entry) = registry.entries.get_mut(owner) else {
        return Ok(());
    };
    anyhow::ensure!(entry.closing, "OpenCode session instance is not closing");
    anyhow::ensure!(
        entry.in_flight == 0,
        "OpenCode session shutdown finished with {} in-flight operation(s)",
        entry.in_flight
    );
    entry.cleanup_complete = true;
    registry.entries.remove(owner);
    Ok(())
}

fn session_instance_is_closing(owner: &SessionInstance) -> bool {
    lifecycle_registry()
        .lock()
        .unwrap()
        .entries
        .get(owner)
        .is_some_and(|entry| entry.closing)
}

struct LifecyclePermit {
    owner: SessionInstance,
}

impl Drop for LifecyclePermit {
    fn drop(&mut self) {
        let (drain_notify, should_remove) = {
            let mut registry = lifecycle_registry().lock().unwrap();
            let Some(entry) = registry.entries.get_mut(&self.owner) else {
                return;
            };
            entry.in_flight = entry.in_flight.saturating_sub(1);
            let drain_notify = (entry.in_flight == 0).then(|| Arc::clone(&entry.drain_notify));
            let should_remove = entry.cleanup_complete && entry.in_flight == 0;
            (drain_notify, should_remove)
        };
        if let Some(drain_notify) = drain_notify {
            drain_notify.notify_waiters();
        }
        if should_remove {
            let mut registry = lifecycle_registry().lock().unwrap();
            if registry
                .entries
                .get(&self.owner)
                .is_some_and(|entry| entry.cleanup_complete && entry.in_flight == 0)
            {
                registry.entries.remove(&self.owner);
            }
        }
    }
}

async fn wait_for_session_inflight_drain(owner: &SessionInstance, timeout: Duration) -> Result<()> {
    let drain = async {
        loop {
            let notified = {
                let registry = lifecycle_registry().lock().unwrap();
                let Some(entry) = registry.entries.get(owner) else {
                    return Ok::<(), anyhow::Error>(());
                };
                anyhow::ensure!(
                    entry.closing,
                    "cannot drain OpenCode operations for a session instance that is not closing"
                );
                if entry.in_flight == 0 {
                    return Ok(());
                }
                Arc::clone(&entry.drain_notify).notified_owned()
            };
            notified.await;
        }
    };

    tokio::time::timeout(timeout, drain)
        .await
        .with_context(|| {
            format!(
                "OpenCode session shutdown timed out waiting for in-flight operations to drain (session {})",
                owner.id
            )
        })??;
    Ok(())
}

pub(crate) fn ensure_session_replacement_allowed(session_id: &str) -> Result<()> {
    let registry = lifecycle_registry().lock().unwrap();
    anyhow::ensure!(
        !registry.entries.keys().any(|owner| owner.id == session_id),
        "OpenCode session {session_id} is still draining its previous instance"
    );
    Ok(())
}

async fn ensure_current_active_instance(
    owner: &SessionInstance,
    session: &config::Session,
) -> Result<LifecyclePermit> {
    anyhow::ensure!(
        owner.matches(session),
        "OpenCode operation session snapshot does not match its owner instance"
    );
    anyhow::ensure!(
        !session_instance_is_closing(owner),
        "OpenCode session instance is closing"
    );
    let current = config::read_session_metadata(&owner.id)
        .await
        .with_context(|| {
            format!(
                "cannot verify current OpenCode session instance {}",
                owner.id
            )
        })?;
    anyhow::ensure!(
        owner.matches(&current),
        "OpenCode session instance is no longer current"
    );
    anyhow::ensure!(
        config::session_is_active(&owner.id).await?,
        "OpenCode session instance is not active"
    );

    let mut registry = lifecycle_registry().lock().unwrap();
    let entry = registry
        .entries
        .entry(owner.clone())
        .or_insert_with(LifecycleEntry::new);
    anyhow::ensure!(
        !entry.closing,
        "OpenCode session instance began closing while it was being verified"
    );
    entry.in_flight += 1;
    Ok(LifecyclePermit {
        owner: owner.clone(),
    })
}

impl SessionInstance {
    fn from_session(session: &config::Session) -> Self {
        Self {
            id: session.id.clone(),
            started_at: session.started_at,
            process_id: session.process_id,
        }
    }

    fn matches(&self, session: &config::Session) -> bool {
        self.id == session.id
            && self.started_at == session.started_at
            && self.process_id == session.process_id
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TaskStatus {
    Accepted,
    Running,
    WaitingApproval,
    RetryableFailed,
    Completed,
    Interrupted,
    Failed,
    ReconciliationRequired,
    Unknown,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::RetryableFailed => "retryable_failed",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::Unknown => "unknown",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OperationPhase {
    Accepted,
    Applied,
    RetryableFailed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct OperationOutcome {
    status: TaskStatus,
    revision: u64,
    generation: u64,
    opencode_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct OperationReceipt {
    operation_id: Uuid,
    request_fingerprint: Uuid,
    action: String,
    phase: OperationPhase,
    outcome: OperationOutcome,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct OperationTombstone {
    operation_id: Uuid,
    request_fingerprint: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct TaskRecord {
    schema_version: u64,
    task_id: Uuid,
    owner: SessionInstance,
    scope_cwd: PathBuf,
    model: Option<String>,
    agent: Option<String>,
    variant: Option<String>,
    status: TaskStatus,
    revision: u64,
    generation: u64,
    opencode_session_id: Option<String>,
    #[serde(default)]
    usage: Option<BTreeMap<String, u64>>,
    #[serde(default)]
    observed_model: Option<String>,
    #[serde(default)]
    report: Option<Value>,
    #[serde(default)]
    last_error: Option<String>,
    created_at: u64,
    updated_at: u64,
    operations: Vec<OperationReceipt>,
    #[serde(default)]
    operation_tombstones: Vec<OperationTombstone>,
}

impl TaskRecord {
    fn outcome(&self) -> OperationOutcome {
        OperationOutcome {
            status: self.status,
            revision: self.revision,
            generation: self.generation,
            opencode_session_id: self.opencode_session_id.clone(),
        }
    }
}

fn update_operation_receipt(record: &mut TaskRecord, operation_id: Uuid, phase: OperationPhase) {
    let outcome = record.outcome();
    if let Some(receipt) = record
        .operations
        .iter_mut()
        .find(|receipt| receipt.operation_id == operation_id)
    {
        receipt.phase = phase;
        receipt.outcome = outcome;
    }
}

#[derive(Clone, Debug)]
struct TaskStore {
    directory: PathBuf,
}

enum StartAcceptance {
    Existing(TaskRecord),
    Accepted(TaskRecord, TaskRuntimeLease),
}

#[derive(Debug)]
enum ControlAcceptance {
    Replay(Value),
    Accepted(Box<TaskRecord>, Option<TaskRuntimeLease>),
}

enum RuntimeAccess {
    Local,
    Acquired(TaskRuntimeLease),
    OwnedElsewhere,
}

struct FinalizeOwnerOutcome {
    finalized: usize,
    deferred: bool,
}

#[derive(Debug)]
struct TaskStoreGuard {
    _process: MutexGuard<'static, ()>,
    file: File,
}

impl Drop for TaskStoreGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: file remains open for the lifetime of the lock.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[derive(Debug)]
struct TaskRuntimeLease {
    file: File,
}

impl Drop for TaskRuntimeLease {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: file remains open for the lifetime of the lease.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl TaskStore {
    fn default_store() -> Result<Self> {
        Ok(Self {
            directory: config::state_dir()?.join("opencode-tasks"),
        })
    }

    #[cfg(test)]
    fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn ensure_directory(&self) -> Result<()> {
        if let Some(parent) = self.directory.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => validate_store_directory(&self.directory, &metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_directory(&self.directory)?;
                let metadata = std::fs::symlink_metadata(&self.directory)?;
                validate_store_directory(&self.directory, &metadata)
            }
            Err(error) => Err(error).context("cannot inspect OpenCode task store"),
        }
    }

    fn lock(&self) -> Result<TaskStoreGuard> {
        let process = store_lock().lock().unwrap();
        self.ensure_directory()?;
        let path = self.directory.join(".store.lock");
        let file = open_private_lock_file(&path)?;
        #[cfg(unix)]
        {
            // SAFETY: flock is called with a valid open descriptor. The guard
            // keeps it open until the critical section ends.
            let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if status != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("cannot lock OpenCode task store");
            }
        }
        Ok(TaskStoreGuard {
            _process: process,
            file,
        })
    }

    fn runtime_lock_directory(&self) -> PathBuf {
        self.directory.join("runtime-locks")
    }

    fn runtime_lock_path(&self, task_id: Uuid) -> PathBuf {
        self.runtime_lock_directory()
            .join(format!("{task_id}.lock"))
    }

    fn runtime_state_root(&self) -> PathBuf {
        self.directory.join("runtime-state")
    }

    fn runtime_state_directory(&self, task_id: Uuid) -> PathBuf {
        self.runtime_state_root().join(task_id.to_string())
    }

    fn try_acquire_runtime_lease(&self, task_id: Uuid) -> Result<Option<TaskRuntimeLease>> {
        let _guard = self.lock()?;
        self.try_acquire_runtime_lease_locked(task_id)
    }

    fn try_acquire_runtime_lease_locked(&self, task_id: Uuid) -> Result<Option<TaskRuntimeLease>> {
        ensure_private_directory(&self.runtime_lock_directory())?;
        let file = open_private_lock_file(&self.runtime_lock_path(task_id))?;
        #[cfg(unix)]
        {
            // SAFETY: flock is called with a valid open descriptor. A
            // successful descriptor is retained by TaskRuntimeLease.
            let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if status != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EWOULDBLOCK)
                    || error.raw_os_error() == Some(libc::EAGAIN)
                {
                    return Ok(None);
                }
                return Err(error).context("cannot lock OpenCode task runtime");
            }
        }
        Ok(Some(TaskRuntimeLease { file }))
    }

    fn runtime_lease_held_locked(&self, task_id: Uuid) -> Result<bool> {
        let path = self.runtime_lock_path(task_id);
        let file = match open_existing_private_lock_file(&path) {
            Ok(file) => file,
            Err(error) if is_not_found(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        {
            // SAFETY: flock is called with a valid open descriptor and is
            // immediately released when this inspection returns.
            let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if status == 0 {
                let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
                return Ok(false);
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK)
                || error.raw_os_error() == Some(libc::EAGAIN)
            {
                return Ok(true);
            }
            Err(error).context("cannot inspect OpenCode task runtime lock")
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Ok(false)
        }
    }

    fn path(&self, task_id: Uuid) -> PathBuf {
        self.directory.join(format!("{task_id}.json"))
    }

    fn load_for_reconciliation(
        &self,
        session: &config::Session,
        task_id: Uuid,
    ) -> Result<(TaskRecord, RuntimeAccess)> {
        let _guard = self.lock()?;
        let record = self.load_locked(session, task_id)?;
        let access = if runtime_matches_record(&record) {
            RuntimeAccess::Local
        } else {
            match self.try_acquire_runtime_lease_locked(task_id)? {
                Some(lease) => RuntimeAccess::Acquired(lease),
                None => RuntimeAccess::OwnedElsewhere,
            }
        };
        Ok((record, access))
    }

    fn load_locked(&self, session: &config::Session, task_id: Uuid) -> Result<TaskRecord> {
        let record = self.read_record(task_id).map_err(|error| {
            if is_not_found(&error) {
                anyhow::anyhow!("OPENCODE_TASK_NOT_FOUND: task was not found")
            } else {
                error
            }
        })?;
        ensure_task_owner(&record, session)?;
        Ok(record)
    }

    fn save_locked(&self, record: &TaskRecord) -> Result<()> {
        self.ensure_directory()?;
        validate_record(record)?;
        self.prune_locked(record)?;
        let bytes = serde_json::to_vec_pretty(record)?;
        anyhow::ensure!(
            bytes.len() <= MAX_TASK_RECORD_BYTES,
            "OpenCode task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let path = self.path(record.task_id);
        reject_symlink_target(&path)?;
        let temporary = self
            .directory
            .join(format!(".{}.{}.tmp", record.task_id, Uuid::new_v4()));
        let mut cleanup = TemporaryFile::new(temporary.clone());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        reject_symlink_target(&path)?;
        std::fs::rename(&temporary, &path)?;
        cleanup.disarm();
        if let Ok(directory) = File::open(&self.directory) {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    fn update<F>(&self, session: &config::Session, task_id: Uuid, f: F) -> Result<TaskRecord>
    where
        F: FnOnce(&mut TaskRecord) -> Result<()>,
    {
        let _guard = self.lock()?;
        let mut record = self.load_locked(session, task_id)?;
        f(&mut record)?;
        record.updated_at = config::unix_time();
        self.save_locked(&record)?;
        Ok(record)
    }

    fn update_if_instance_live<F>(
        &self,
        session: &config::Session,
        task_id: Uuid,
        owner: &SessionInstance,
        f: F,
    ) -> Result<TaskRecord>
    where
        F: FnOnce(&mut TaskRecord) -> Result<()>,
    {
        let _guard = self.lock()?;
        let registry = lifecycle_registry().lock().unwrap();
        let entry = registry
            .entries
            .get(owner)
            .context("OpenCode session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "OpenCode session instance is closing");
        let mut record = self.load_locked(session, task_id)?;
        f(&mut record)?;
        record.updated_at = config::unix_time();
        self.save_locked(&record)?;
        Ok(record)
    }

    #[cfg(test)]
    fn accept_start(
        &self,
        session: &config::Session,
        record: TaskRecord,
    ) -> Result<StartAcceptance> {
        let _guard = self.lock()?;
        self.accept_start_locked(session, record)
    }

    fn accept_start_if_instance_live(
        &self,
        session: &config::Session,
        record: TaskRecord,
        owner: &SessionInstance,
    ) -> Result<StartAcceptance> {
        let _guard = self.lock()?;
        let registry = lifecycle_registry().lock().unwrap();
        let entry = registry
            .entries
            .get(owner)
            .context("OpenCode session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "OpenCode session instance is closing");
        self.accept_start_locked(session, record)
    }

    fn accept_start_locked(
        &self,
        session: &config::Session,
        record: TaskRecord,
    ) -> Result<StartAcceptance> {
        let candidate_receipt = record
            .operations
            .first()
            .context("OpenCode start acceptance is missing its operation receipt")?;
        match self.read_record(record.task_id) {
            Ok(mut existing) => {
                ensure_task_owner(&existing, session)?;
                let retryable = existing
                    .operations
                    .iter()
                    .find(|receipt| receipt.operation_id == candidate_receipt.operation_id)
                    .is_some_and(|receipt| {
                        receipt.request_fingerprint == candidate_receipt.request_fingerprint
                            && receipt.action == "start"
                            && receipt.phase == OperationPhase::RetryableFailed
                            && existing.status == TaskStatus::RetryableFailed
                            && existing.opencode_session_id.is_none()
                    });
                if retryable {
                    let lease = self
                        .try_acquire_runtime_lease_locked(existing.task_id)?
                        .context(
                            "OPENCODE_TASK_RUNTIME_OWNED: task runtime belongs to another Temote process",
                        )?;
                    existing.status = TaskStatus::Accepted;
                    existing.revision = existing.revision.saturating_add(1);
                    update_operation_receipt(
                        &mut existing,
                        candidate_receipt.operation_id,
                        OperationPhase::Accepted,
                    );
                    existing.updated_at = config::unix_time();
                    self.save_locked(&existing)?;
                    Ok(StartAcceptance::Accepted(existing, lease))
                } else {
                    Ok(StartAcceptance::Existing(existing))
                }
            }
            Err(error) if is_not_found(&error) => {
                let lease = self
                    .try_acquire_runtime_lease_locked(record.task_id)?
                    .context(
                        "OPENCODE_TASK_RUNTIME_OWNED: task runtime belongs to another Temote process",
                    )?;
                if let Err(error) = self.save_locked(&record) {
                    drop(lease);
                    match std::fs::remove_file(self.runtime_lock_path(record.task_id)) {
                        Ok(()) => {}
                        Err(cleanup_error)
                            if cleanup_error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(cleanup_error) => {
                            return Err(error).context(format!(
                                "cannot remove unused OpenCode runtime lock: {cleanup_error}"
                            ));
                        }
                    }
                    return Err(error);
                }
                Ok(StartAcceptance::Accepted(record, lease))
            }
            Err(error) => Err(error),
        }
    }

    fn accept_control_if_instance_live(
        &self,
        session: &config::Session,
        task_id: Uuid,
        operation_id: Uuid,
        request_fingerprint: Uuid,
        action: &str,
    ) -> Result<ControlAcceptance> {
        let _guard = self.lock()?;
        let registry = lifecycle_registry().lock().unwrap();
        let owner = SessionInstance::from_session(session);
        let entry = registry
            .entries
            .get(&owner)
            .context("OpenCode session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "OpenCode session instance is closing");
        self.accept_control_locked(session, task_id, operation_id, request_fingerprint, action)
    }

    fn accept_control_locked(
        &self,
        session: &config::Session,
        task_id: Uuid,
        operation_id: Uuid,
        request_fingerprint: Uuid,
        action: &str,
    ) -> Result<ControlAcceptance> {
        let mut record = self.load_locked(session, task_id)?;
        if let Some(receipt) = record
            .operations
            .iter()
            .find(|receipt| receipt.operation_id == operation_id)
        {
            anyhow::ensure!(
                receipt.request_fingerprint == request_fingerprint,
                "OPERATION_CONFLICT: operation_id was already accepted with a different request"
            );
            if receipt.phase == OperationPhase::Accepted {
                let mut outcome = receipt.outcome.clone();
                outcome.status = TaskStatus::ReconciliationRequired;
                return Ok(ControlAcceptance::Replay(operation_view(task_id, &outcome)));
            }
            return Ok(ControlAcceptance::Replay(operation_view(
                task_id,
                &receipt.outcome,
            )));
        }
        if let Some(tombstone) = record
            .operation_tombstones
            .iter()
            .find(|tombstone| tombstone.operation_id == operation_id)
        {
            anyhow::ensure!(
                tombstone.request_fingerprint == request_fingerprint,
                "OPERATION_CONFLICT: operation_id was already accepted with a different request"
            );
            anyhow::bail!(
                "OPERATION_REPLAY_COMPACTED: operation_id was accepted during task retention, but its detailed receipt was compacted; refusing to reapply the mutation"
            );
        }

        if action == "resume" {
            anyhow::ensure!(
                matches!(
                    record.status,
                    TaskStatus::Unknown
                        | TaskStatus::ReconciliationRequired
                        | TaskStatus::RetryableFailed
                ),
                "OpenCode task does not require resume reconciliation"
            );
        } else {
            anyhow::ensure!(
                matches!(
                    record.status,
                    TaskStatus::Running | TaskStatus::WaitingApproval
                ),
                "OpenCode task is not active and cannot be controlled"
            );
        }
        anyhow::ensure!(
            record.opencode_session_id.is_some() || action == "resume",
            "OpenCode task requires reconciliation before control"
        );

        let runtime_lease = if runtime_matches_record(&record) {
            None
        } else {
            Some(self.try_acquire_runtime_lease_locked(task_id)?.context(
                "OPENCODE_TASK_RUNTIME_OWNED: task runtime belongs to another Temote process",
            )?)
        };

        record.revision = record.revision.saturating_add(1);
        let accepted_outcome = record.outcome();
        if record.operations.len() >= MAX_OPERATION_HISTORY {
            let receipt = record.operations.remove(0);
            record.operation_tombstones.push(OperationTombstone {
                operation_id: receipt.operation_id,
                request_fingerprint: receipt.request_fingerprint,
            });
        }
        record.operations.push(OperationReceipt {
            operation_id,
            request_fingerprint,
            action: action.to_owned(),
            phase: OperationPhase::Accepted,
            outcome: accepted_outcome,
        });
        self.save_locked(&record)?;
        Ok(ControlAcceptance::Accepted(Box::new(record), runtime_lease))
    }

    fn read_record(&self, task_id: Uuid) -> Result<TaskRecord> {
        let path = self.path(task_id);
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let file = options.open(&path)?;
        let metadata = file.metadata()?;
        validate_private_regular_file(&path, &metadata)?;
        anyhow::ensure!(
            metadata.len() <= MAX_TASK_RECORD_BYTES as u64,
            "OpenCode task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_TASK_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= MAX_TASK_RECORD_BYTES,
            "OpenCode task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let record: TaskRecord =
            serde_json::from_slice(&bytes).context("invalid OpenCode task record")?;
        validate_record(&record)?;
        anyhow::ensure!(
            record.task_id == task_id,
            "OpenCode task record ID mismatch"
        );
        Ok(record)
    }

    fn prune_locked(&self, current: &TaskRecord) -> Result<()> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("cannot list OpenCode task store"),
        };
        let now = config::unix_time();
        let mut scoped = Vec::new();
        let mut count = 0usize;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "OpenCode task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
            );
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(stem) else {
                continue;
            };
            let Ok(record) = self.read_record(id) else {
                continue;
            };
            let expired = now.saturating_sub(record.updated_at) >= TASK_RETENTION_SECONDS;
            let runtime_backed = runtime_matches_record(&record)
                || self.runtime_lease_held_locked(record.task_id)?;
            let terminal = record.status.is_terminal();
            if expired && terminal && !runtime_backed && id != current.task_id {
                std::fs::remove_file(entry.path())
                    .with_context(|| format!("cannot prune expired OpenCode task {id}"))?;
                match std::fs::remove_file(self.runtime_lock_path(id)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("cannot prune OpenCode runtime lock {id}"));
                    }
                }
                let state_dir = self.runtime_state_directory(id);
                if state_dir.exists()
                    && let Err(error) = std::fs::remove_dir_all(&state_dir)
                {
                    return Err(error)
                        .with_context(|| format!("cannot prune OpenCode runtime state {id}"));
                }
                continue;
            }
            if record.owner == current.owner && record.scope_cwd == current.scope_cwd {
                scoped.push(id);
            }
        }
        let current_is_persisted = scoped.contains(&current.task_id);
        let projected = scoped.len() + usize::from(!current_is_persisted);
        if projected > MAX_TASKS_PER_SCOPE {
            anyhow::bail!(
                "OpenCode task scope has reached its retention limit; refusing to accept another task"
            );
        }
        Ok(())
    }

    fn finalize_owner(&self, owner: &SessionInstance) -> Result<FinalizeOwnerOutcome> {
        let _guard = self.lock()?;
        let metadata = match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FinalizeOwnerOutcome {
                    finalized: 0,
                    deferred: false,
                });
            }
            Err(error) => return Err(error).context("cannot inspect OpenCode task store"),
        };
        validate_store_directory(&self.directory, &metadata)?;
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FinalizeOwnerOutcome {
                    finalized: 0,
                    deferred: false,
                });
            }
            Err(error) => return Err(error).context("cannot list OpenCode task store"),
        };
        let now = config::unix_time();
        let mut count = 0usize;
        let mut finalized = 0usize;
        let mut deferred = false;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "OpenCode task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
            );
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(stem) else {
                continue;
            };
            let mut record = match self.read_record(id) {
                Ok(record) => record,
                Err(error) if is_not_found(&error) => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("cannot inspect OpenCode task {id} during cleanup")
                    });
                }
            };
            if record.owner != owner.clone() {
                continue;
            }
            if self.runtime_lease_held_locked(record.task_id)? {
                deferred = true;
                continue;
            }
            if record.status.is_terminal() {
                continue;
            }

            record.status = TaskStatus::Interrupted;
            record.revision = record.revision.saturating_add(1);
            record.updated_at = now;
            let outcome = record.outcome();
            for receipt in &mut record.operations {
                if receipt.phase == OperationPhase::Accepted {
                    receipt.phase = OperationPhase::Applied;
                }
                receipt.outcome = outcome.clone();
            }
            self.save_locked(&record)?;
            finalized += 1;
        }
        Ok(FinalizeOwnerOutcome {
            finalized,
            deferred,
        })
    }
}

struct TemporaryFile {
    path: PathBuf,
    armed: bool,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn validate_store_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "OpenCode task store must be a real directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "OpenCode task store must be owner-only (mode {mode:04o})"
        );
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_store_directory(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(path)?;
            let metadata = std::fs::symlink_metadata(path)?;
            validate_store_directory(path, &metadata)
        }
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect private directory {}", path.display())),
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("cannot create private directory {}", path.display())),
    }
}

fn open_private_lock_file(path: &Path) -> Result<File> {
    reject_symlink_target(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("cannot open private lock file {}", path.display()))?;
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let metadata = file.metadata()?;
    validate_private_regular_file(path, &metadata)?;
    Ok(file)
}

fn open_existing_private_lock_file(path: &Path) -> Result<File> {
    reject_symlink_target(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("cannot open private lock file {}", path.display()))?;
    let metadata = file.metadata()?;
    validate_private_regular_file(path, &metadata)?;
    Ok(file)
}

fn validate_private_regular_file(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "OpenCode task path is not a regular file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(mode & 0o077 == 0, "OpenCode task file must be owner-only");
    }
    Ok(())
}

fn reject_symlink_target(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "OpenCode task path may not be a symlink"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect OpenCode task path"),
    }
    Ok(())
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

fn ensure_task_owner(record: &TaskRecord, session: &config::Session) -> Result<()> {
    let scope = config::canonical_directory(&session.cwd)?;
    anyhow::ensure!(
        record.owner.matches(session) && record.scope_cwd == scope,
        "OPENCODE_TASK_NOT_FOUND: task was not found"
    );
    Ok(())
}

fn validate_record(record: &TaskRecord) -> Result<()> {
    anyhow::ensure!(
        record.schema_version == TASK_SCHEMA_VERSION,
        "unsupported OpenCode task schema version"
    );
    config::validate_session_id(&record.owner.id)?;
    let canonical = config::canonical_directory(&record.scope_cwd)?;
    anyhow::ensure!(
        canonical == record.scope_cwd,
        "OpenCode task scope is not canonical"
    );
    if let Some(model) = &record.model {
        validate_argument(model, "model")?;
    }
    if let Some(agent) = &record.agent {
        validate_argument(agent, "agent")?;
    }
    if let Some(variant) = &record.variant {
        validate_argument(variant, "variant")?;
    }
    if let Some(error) = &record.last_error {
        anyhow::ensure!(
            error.len() <= MAX_ERROR_BYTES,
            "OpenCode task error field exceeds {MAX_ERROR_BYTES} bytes"
        );
    }
    if let Some(report) = &record.report {
        let bytes = serde_json::to_vec(report)?;
        anyhow::ensure!(
            bytes.len() <= MAX_REPORT_BYTES,
            "OpenCode task report exceeds {MAX_REPORT_BYTES} bytes"
        );
    }
    anyhow::ensure!(
        record.revision > 0,
        "OpenCode task revision must be positive"
    );
    anyhow::ensure!(
        record.operations.len() <= MAX_OPERATION_HISTORY,
        "OpenCode task operation history exceeds limit"
    );
    let mut operation_ids = record
        .operations
        .iter()
        .map(|receipt| receipt.operation_id)
        .collect::<BTreeSet<_>>();
    for tombstone in &record.operation_tombstones {
        anyhow::ensure!(
            operation_ids.insert(tombstone.operation_id),
            "OpenCode task operation history contains a duplicate operation_id"
        );
    }
    if let Some(usage) = &record.usage {
        anyhow::ensure!(
            usage.len() <= 16 && usage.values().all(|value| *value <= u64::MAX / 2),
            "OpenCode task usage has unsupported fields"
        );
    }
    Ok(())
}

fn validate_argument(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_ARGUMENT_BYTES && !value.contains('\0'),
        "{label} must contain 1..={MAX_ARGUMENT_BYTES} NUL-free UTF-8 bytes"
    );
    Ok(())
}

fn validate_task_input(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_TASK_INPUT_BYTES && !value.contains('\0'),
        "{label} must contain 1..={MAX_TASK_INPUT_BYTES} NUL-free UTF-8 bytes"
    );
    Ok(())
}

#[cfg(unix)]
fn append_scope_identity(bytes: &mut Vec<u8>, scope: &Path) {
    use std::os::unix::ffi::OsStrExt;
    bytes.extend_from_slice(scope.as_os_str().as_bytes());
}

#[cfg(not(unix))]
fn append_scope_identity(bytes: &mut Vec<u8>, scope: &Path) {
    bytes.extend_from_slice(scope.to_string_lossy().as_bytes());
}

fn task_id_for_operation(session: &config::Session, operation_id: Uuid) -> Result<Uuid> {
    let scope = config::canonical_directory(&session.cwd)?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(session.id.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&session.started_at.to_le_bytes());
    bytes.extend_from_slice(&session.process_id.to_le_bytes());
    append_scope_identity(&mut bytes, &scope);
    bytes.push(0);
    bytes.extend_from_slice(operation_id.as_bytes());
    Ok(Uuid::new_v5(&TASK_ID_NAMESPACE, &bytes))
}

fn prompt_message_id(operation_id: Uuid) -> String {
    // Both serve contracts require the "msg_" prefix: the V2 API validates
    // `msg_` strictly and the V1 surface accepts it as a "msg"-prefixed id.
    format!(
        "msg_{}",
        Uuid::new_v5(&MESSAGE_ID_NAMESPACE, operation_id.as_bytes()).simple()
    )
}

fn fingerprint(value: &Value) -> Result<Uuid> {
    let bytes = serde_json::to_vec(value)?;
    Ok(Uuid::new_v5(&REQUEST_FINGERPRINT_NAMESPACE, &bytes))
}

fn operation_view(task_id: Uuid, outcome: &OperationOutcome) -> Value {
    json!({
        "task_id": task_id,
        "status": outcome.status.as_str(),
        "revision": outcome.revision,
        "generation": outcome.generation,
        "opencode_session_id": outcome.opencode_session_id,
        "reconciliation_required": outcome.status == TaskStatus::ReconciliationRequired,
    })
}

fn task_view(record: &TaskRecord, evidence_ref: Option<&evidence::EvidenceRef>) -> Value {
    json!({
        "task_id": record.task_id,
        "status": record.status.as_str(),
        "revision": record.revision,
        "generation": record.generation,
        "model": record.model,
        "agent": record.agent,
        "variant": record.variant,
        "opencode_session_id": record.opencode_session_id,
        "usage": record.usage,
        "observed_model": record.observed_model,
        "report": record.report,
        "last_error": record.last_error,
        "reconciliation_required": record.status == TaskStatus::ReconciliationRequired,
        "evidence": evidence_ref,
        "retention_seconds": TASK_RETENTION_SECONDS,
    })
}

fn task_view_at_revision(record: &TaskRecord, after_revision: Option<u64>) -> Value {
    if after_revision == Some(record.revision) {
        return json!({
            "task_id": record.task_id,
            "status": "not_modified",
            "revision": record.revision,
            "last_updated_at": record.updated_at,
            "reconciliation_deferred": true,
        });
    }
    let mut view = task_view(record, None);
    if let Some(object) = view.as_object_mut() {
        object.insert("last_updated_at".to_owned(), json!(record.updated_at));
        object.insert("reconciliation_deferred".to_owned(), json!(true));
    }
    view
}

fn store_evidence_for_instance(
    owner: &SessionInstance,
    session: &config::Session,
    response: &Value,
) -> Option<evidence::EvidenceRef> {
    let registry = lifecycle_registry().lock().unwrap();
    if registry
        .entries
        .get(owner)
        .is_none_or(|entry| entry.closing)
    {
        return None;
    }
    evidence::store(
        &session.id,
        &session.cwd,
        serde_json::to_string(response).ok()?,
    )
    .ok()
    .flatten()
}

// ---------- opencode serve transport ----------

/// Transport seam over the OpenCode HTTP surface. The real implementation is
/// an `unofficial-opencode-sdk` V1 client against a per-task `opencode serve`
/// child; tests use an in-memory fake so no network or process is required.
enum ServeClient {
    Sdk(Arc<SdkServe>),
    SdkV2(Arc<SdkV2Serve>),
    #[cfg(test)]
    Fake(Arc<std::sync::Mutex<FakeServe>>),
}

impl Clone for ServeClient {
    fn clone(&self) -> Self {
        match self {
            Self::Sdk(inner) => Self::Sdk(Arc::clone(inner)),
            Self::SdkV2(inner) => Self::SdkV2(Arc::clone(inner)),
            #[cfg(test)]
            Self::Fake(inner) => Self::Fake(Arc::clone(inner)),
        }
    }
}

struct SdkServe {
    client: unofficial_opencode_sdk::Client,
    child: tokio::sync::Mutex<tokio::process::Child>,
    tail: Arc<Mutex<String>>,
}

impl SdkServe {
    async fn kill(&self) {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

struct SdkV2Serve {
    client: unofficial_opencode_sdk::v2::Client,
    child: tokio::sync::Mutex<tokio::process::Child>,
    tail: Arc<Mutex<String>>,
}

impl SdkV2Serve {
    async fn kill(&self) {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

fn v2_model_ref(body: &Value) -> Option<unofficial_opencode_sdk::v2::ModelRef> {
    let model = body.get("model")?;
    let provider = model
        .get("providerID")
        .or_else(|| model.get("provider_id"))
        .and_then(Value::as_str)?;
    let model_id = model
        .get("modelID")
        .or_else(|| model.get("model_id"))
        .or_else(|| model.get("id"))
        .and_then(Value::as_str)?;
    Some(unofficial_opencode_sdk::v2::ModelRef {
        id: model_id.to_owned(),
        provider_id: provider.to_owned(),
        variant: body
            .get("variant")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Bound every SDK request: a request sent while the child is starting or
/// racing a connection teardown can otherwise wait forever.
async fn sdk_call<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = std::result::Result<T, unofficial_opencode_sdk::Error>>,
{
    match tokio::time::timeout(SERVE_REQUEST_TIMEOUT, future).await {
        Ok(result) => Ok(result?),
        Err(_) => anyhow::bail!(
            "opencode serve request exceeded {}s",
            SERVE_REQUEST_TIMEOUT.as_secs()
        ),
    }
}

impl ServeClient {
    async fn health(&self) -> Result<Value> {
        match self {
            Self::Sdk(inner) => {
                let health = sdk_call(inner.client.global().health())
                    .await
                    .context("opencode serve health check failed")?;
                Ok(json!({
                    "healthy": health.healthy,
                    "version": health.version,
                }))
            }
            Self::SdkV2(inner) => {
                let health = sdk_call(inner.client.health().get())
                    .await
                    .context("opencode serve health check failed")?;
                Ok(json!({
                    "healthy": health.healthy,
                    "contract": "v2",
                }))
            }
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().health(),
        }
    }

    async fn provider_list(&self) -> Result<Value> {
        match self {
            Self::Sdk(inner) => sdk_call(inner.client.provider().list())
                .await
                .context("opencode provider list failed"),
            Self::SdkV2(inner) => {
                let providers = sdk_call(inner.client.provider().list(None))
                    .await
                    .context("opencode provider list failed")?;
                let all: Vec<Value> = providers
                    .data
                    .iter()
                    .map(|provider| {
                        json!({
                            "id": provider.id,
                            "name": provider.name,
                            "disabled": provider.disabled,
                        })
                    })
                    .collect();
                Ok(json!({"all": all}))
            }
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().provider_list(),
        }
    }

    async fn session_create(&self, body: &Value) -> Result<Value> {
        match self {
            Self::Sdk(inner) => {
                let request: unofficial_opencode_sdk::CreateSessionRequest =
                    serde_json::from_value(body.clone())
                        .context("invalid session create request")?;
                let session = sdk_call(inner.client.session().create(&request))
                    .await
                    .context("opencode session create failed")?;
                Ok(serde_json::to_value(&session)?)
            }
            Self::SdkV2(inner) => {
                let request = unofficial_opencode_sdk::v2::CreateSessionRequest {
                    agent: body.get("agent").and_then(Value::as_str).map(str::to_owned),
                    model: v2_model_ref(body),
                    ..Default::default()
                };
                let session = sdk_call(inner.client.session().create(&request))
                    .await
                    .context("opencode session create failed")?;
                Ok(serde_json::to_value(&session)?)
            }
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().session_create(body),
        }
    }

    async fn prompt_async(&self, session_id: &str, body: &Value) -> Result<()> {
        match self {
            Self::Sdk(inner) => {
                let request: unofficial_opencode_sdk::PromptRequest =
                    serde_json::from_value(body.clone()).context("invalid prompt request")?;
                sdk_call(inner.client.session().prompt_async(session_id, &request))
                    .await
                    .context("opencode prompt_async failed")
            }
            Self::SdkV2(inner) => {
                if let Some(model) = v2_model_ref(body) {
                    sdk_call(inner.client.session().switch_model(session_id, &model))
                        .await
                        .context("opencode switch_model failed")?;
                }
                if let Some(agent) = body.get("agent").and_then(Value::as_str) {
                    sdk_call(inner.client.session().switch_agent(session_id, agent))
                        .await
                        .context("opencode switch_agent failed")?;
                }
                let text = body
                    .get("parts")
                    .and_then(Value::as_array)
                    .and_then(|parts| {
                        parts.iter().find_map(|part| {
                            (part.get("type").and_then(Value::as_str) == Some("text"))
                                .then(|| part.get("text").and_then(Value::as_str))
                                .flatten()
                        })
                    })
                    .context("opencode prompt request has no text part")?;
                let request = unofficial_opencode_sdk::v2::PromptRequest {
                    id: body
                        .get("messageID")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    prompt: Some(unofficial_opencode_sdk::v2::PromptInput::text(
                        text.to_owned(),
                    )),
                    delivery: Some(unofficial_opencode_sdk::v2::Delivery::Queue),
                    ..Default::default()
                };
                sdk_call(inner.client.session().prompt(session_id, &request))
                    .await
                    .context("opencode prompt failed")?;
                Ok(())
            }
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().prompt_async(session_id, body),
        }
    }

    async fn abort(&self, session_id: &str) -> Result<()> {
        match self {
            Self::Sdk(inner) => {
                sdk_call(inner.client.session().abort(session_id))
                    .await
                    .context("opencode abort failed")?;
                Ok(())
            }
            Self::SdkV2(inner) => {
                sdk_call(inner.client.session().interrupt(session_id))
                    .await
                    .context("opencode interrupt failed")?;
                Ok(())
            }
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().abort(session_id),
        }
    }

    async fn session_status(&self) -> Result<Value> {
        match self {
            Self::Sdk(inner) => {
                let status = sdk_call(inner.client.session().status())
                    .await
                    .context("opencode session status failed")?;
                Ok(serde_json::to_value(status)?)
            }
            Self::SdkV2(inner) => {
                let active = sdk_call(inner.client.session().active())
                    .await
                    .context("opencode session status failed")?;
                Ok(active
                    .into_keys()
                    .map(|session_id| (session_id, json!({"type": "busy"})))
                    .collect())
            }
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().session_status(),
        }
    }

    async fn messages(&self, session_id: &str, limit: u32) -> Result<Vec<Value>> {
        match self {
            Self::Sdk(inner) => sdk_call(inner.client.session().messages(
                session_id,
                &unofficial_opencode_sdk::current::SessionMessagesOptions {
                    limit: Some(limit),
                    before: None,
                },
            ))
            .await
            .context("opencode messages failed"),
            Self::SdkV2(inner) => {
                let mut page = sdk_call(inner.client.session().messages(
                    session_id,
                    &unofficial_opencode_sdk::v2::SessionMessagesOptions {
                        limit: Some(limit),
                        order: Some(unofficial_opencode_sdk::v2::Order::Desc),
                        cursor: None,
                    },
                ))
                .await
                .context("opencode messages failed")?;
                page.data.reverse();
                Ok(page.data)
            }
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().messages(session_id, limit),
        }
    }

    async fn permission_list(&self) -> Result<Vec<Value>> {
        match self {
            Self::Sdk(inner) => sdk_call(inner.client.permission().list())
                .await
                .context("opencode permission list failed"),
            Self::SdkV2(inner) => {
                let pending = sdk_call(inner.client.permission().request().list(None))
                    .await
                    .context("opencode permission list failed")?;
                pending
                    .data
                    .iter()
                    .map(|request| serde_json::to_value(request).map_err(Into::into))
                    .collect()
            }
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().permission_list(),
        }
    }

    async fn question_list(&self) -> Result<Vec<Value>> {
        match self {
            Self::Sdk(inner) => sdk_call(inner.client.question().list())
                .await
                .context("opencode question list failed"),
            Self::SdkV2(inner) => {
                let pending = sdk_call(inner.client.question().request().list(None))
                    .await
                    .context("opencode question list failed")?;
                pending
                    .data
                    .iter()
                    .map(|request| serde_json::to_value(request).map_err(Into::into))
                    .collect()
            }
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().question_list(),
        }
    }

    async fn shutdown(&self) {
        match self {
            Self::Sdk(inner) => inner.kill().await,
            Self::SdkV2(inner) => inner.kill().await,
            #[cfg(test)]
            Self::Fake(inner) => inner.lock().unwrap().dead = true,
        }
    }

    fn diagnostics_tail(&self) -> Option<String> {
        match self {
            Self::Sdk(inner) => {
                let tail = inner.tail.lock().unwrap().clone();
                (!tail.is_empty()).then_some(tail)
            }
            Self::SdkV2(inner) => {
                let tail = inner.tail.lock().unwrap().clone();
                (!tail.is_empty()).then_some(tail)
            }
            #[cfg(test)]
            Self::Fake(_) => None,
        }
    }
}

/// Spawn a per-task `opencode serve` child on loopback with a per-instance
/// memory-only Basic-auth password and an isolated per-task data directory.
async fn spawn_serve(
    session: &config::Session,
    task_id: Uuid,
    store: &TaskStore,
    _lease: Arc<TaskRuntimeLease>,
    binary: &Path,
) -> Result<ServeClient> {
    let scope = config::canonical_directory(&session.cwd)?;
    ensure_private_directory(&store.runtime_state_root())?;
    let state_dir = store.runtime_state_directory(task_id);
    ensure_private_directory(&state_dir)?;
    let data_dir = state_dir.join("data");
    ensure_private_directory(&data_dir)?;
    #[cfg(test)]
    if let Some(hook) = spawn_hook() {
        return hook(session, task_id);
    }

    // Seed provider auth into the isolated data dir (fresh copy on every spawn
    // so upstream credential refreshes in the real home propagate).
    if let Some(auth_source) = opencode_auth_source() {
        let target = data_dir.join("opencode");
        ensure_private_directory(&target)?;
        let target = target.join("auth.json");
        copy_private_file(&auth_source, &target)
            .with_context(|| "cannot seed OpenCode auth into task state")?;
    }

    let config = crate::local_agent::opencode_config(
        crate::local_agent::Access::WorkspaceWrite,
        &opencode_auth_source().into_iter().collect::<Vec<_>>(),
    )?;
    let config_content = serde_json::to_string(&config)?;

    let contract = serve_contract_override();
    let mut last_error = None;
    for _attempt in 0..SERVE_SPAWN_ATTEMPTS {
        let port = reserve_loopback_port()?;
        let password = Uuid::new_v4().simple().to_string();
        match spawn_serve_once(
            binary,
            &scope,
            &data_dir,
            port,
            &password,
            &config_content,
            contract,
        )
        .await
        {
            Ok(client) => return Ok(client),
            Err(error) => last_error = Some(error),
        }
    }
    drop(_lease);
    Err(last_error.expect("spawn attempts run at least once"))
        .context("cannot start opencode serve for task")
}

fn opencode_auth_source() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    let candidate = base.join("opencode").join("auth.json");
    candidate.is_file().then_some(candidate)
}

fn copy_private_file(source: &Path, target: &Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut input = options.open(source)?;
    let metadata = input.metadata()?;
    anyhow::ensure!(metadata.is_file(), "OpenCode auth source is not a file");
    reject_symlink_target(target)?;
    let mut out_options = OpenOptions::new();
    out_options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    out_options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut output = out_options.open(target)?;
    #[cfg(unix)]
    output.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn reserve_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("cannot reserve a loopback port for opencode serve")?;
    Ok(listener.local_addr()?.port())
}

fn serve_child_env(
    data_dir: &Path,
    password: &str,
    config_content: &str,
) -> Vec<(OsString, OsString)> {
    let mut env = crate::cli::codex::delegation::filtered_child_environment(
        std::env::vars_os(),
        SERVE_CHILD_ENV_ALLOWLIST,
    );
    env.push((
        OsString::from("XDG_DATA_HOME"),
        data_dir.as_os_str().to_os_string(),
    ));
    env.push((
        OsString::from("OPENCODE_SERVER_PASSWORD"),
        OsString::from(password),
    ));
    env.push((
        OsString::from("OPENCODE_CONFIG_CONTENT"),
        OsString::from(config_content),
    ));
    env
}

async fn spawn_serve_once(
    binary: &Path,
    scope: &Path,
    data_dir: &Path,
    port: u16,
    password: &str,
    config_content: &str,
    contract_override: ServeContract,
) -> Result<ServeClient> {
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("serve")
        .arg("--hostname")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .current_dir(scope)
        .env_clear()
        .envs(serve_child_env(data_dir, password, config_content))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot spawn {}", binary.display()))?;

    let tail = Arc::new(Mutex::new(String::new()));
    if let Some(stdout) = child.stdout.take() {
        drain_tail(stdout, Arc::clone(&tail));
    }
    if let Some(stderr) = child.stderr.take() {
        drain_tail(stderr, Arc::clone(&tail));
    }

    let base_url = format!("http://127.0.0.1:{port}/");
    let directory = scope.to_string_lossy().into_owned();
    let client_v1 = unofficial_opencode_sdk::Client::builder()
        .base_url(base_url.clone())
        .password(password)
        .directory(directory.clone())
        .build()
        .context("cannot build OpenCode SDK client")?;
    let client_v2 = unofficial_opencode_sdk::v2::Client::builder()
        .base_url(base_url)
        .password(password)
        .directory(directory)
        .build()
        .context("cannot build OpenCode SDK v2 client")?;

    let child = tokio::sync::Mutex::new(child);
    let deadline = Instant::now() + SERVE_HEALTH_TIMEOUT;
    let mut v1_failed_once = false;
    let mut last_error = None;
    loop {
        // A request sent while the child is still starting can hang without
        // ever completing, so each probe also carries its own timeout to keep
        // the overall deadline effective. Probe the V1 surface first: servers
        // that answer `global/health` keep the established V1 code path, and
        // only servers that fail it (OpenCode 2.x, which serves `api/*`) fall
        // through to the V2 probe. The V2 probe only runs after the V1 probe
        // has failed at least once, and each probe runs sequentially, so a
        // working V1 surface always wins its poll; when both contracts are up
        // either adapter is correct and V1 is preferred on ties.
        if contract_override != ServeContract::V2 {
            match tokio::time::timeout(SERVE_HEALTH_REQUEST_TIMEOUT, client_v1.global().health())
                .await
            {
                Ok(Ok(_)) => {
                    return Ok(ServeClient::Sdk(Arc::new(SdkServe {
                        client: client_v1,
                        child,
                        tail: Arc::clone(&tail),
                    })));
                }
                Ok(Err(error)) => {
                    v1_failed_once = true;
                    last_error = Some(error.to_string());
                }
                Err(_) => {
                    v1_failed_once = true;
                    last_error = Some("opencode serve health probe timed out".to_owned());
                }
            }
        }
        if contract_override != ServeContract::V1
            && (contract_override == ServeContract::V2 || v1_failed_once)
        {
            match tokio::time::timeout(SERVE_HEALTH_REQUEST_TIMEOUT, client_v2.health().get()).await
            {
                Ok(Ok(_)) => {
                    return Ok(ServeClient::SdkV2(Arc::new(SdkV2Serve {
                        client: client_v2,
                        child,
                        tail: Arc::clone(&tail),
                    })));
                }
                Ok(Err(error)) => last_error = Some(error.to_string()),
                Err(_) => last_error = Some("opencode serve v2 health probe timed out".to_owned()),
            }
        }

        {
            let mut child_guard = child.lock().await;
            if let Some(status) = child_guard.try_wait().ok().flatten() {
                let tail = tail.lock().unwrap().clone();
                anyhow::bail!(
                    "opencode serve exited early ({status}): {}",
                    truncate_tail(&tail)
                );
            }
        }
        if Instant::now() >= deadline {
            let tail = tail.lock().unwrap().clone();
            let detail = last_error.unwrap_or_else(|| "no health endpoint answered".to_owned());
            return Err(anyhow::anyhow!(detail).context(format!(
                "opencode serve did not become healthy: {}",
                truncate_tail(&tail)
            )));
        }
        tokio::time::sleep(SERVE_HEALTH_POLL).await;
    }
}

fn drain_tail(
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tail: Arc<Mutex<String>>,
) {
    tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        loop {
            match tokio::io::AsyncReadExt::read(&mut reader, &mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    let chunk = String::from_utf8_lossy(&buffer[..read]);
                    let mut tail = tail.lock().unwrap();
                    tail.push_str(&chunk);
                    if tail.len() > SERVE_TAIL_BYTES {
                        let cut = tail.len() - SERVE_TAIL_BYTES;
                        tail.drain(..cut);
                    }
                }
            }
        }
    });
}

fn truncate_tail(tail: &str) -> String {
    const MAX: usize = 512;
    if tail.len() <= MAX {
        tail.to_owned()
    } else {
        format!("…{}", &tail[tail.len() - MAX..])
    }
}

// ---------- runtime registry ----------

#[derive(Clone)]
struct RuntimeHandle {
    client: ServeClient,
    owner: SessionInstance,
    scope: PathBuf,
    started_at: Instant,
    _lease: Arc<TaskRuntimeLease>,
}

fn runtimes() -> &'static Mutex<HashMap<Uuid, RuntimeHandle>> {
    static RUNTIMES: OnceLock<Mutex<HashMap<Uuid, RuntimeHandle>>> = OnceLock::new();
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime_for(session: &config::Session, task_id: Uuid) -> Option<RuntimeHandle> {
    let owner = SessionInstance::from_session(session);
    let lifecycle = lifecycle_registry().lock().unwrap();
    if lifecycle
        .entries
        .get(&owner)
        .is_some_and(|entry| entry.closing)
    {
        return None;
    }
    let state = runtimes().lock().unwrap();
    let runtime = state.get(&task_id)?;
    if Instant::now().saturating_duration_since(runtime.started_at) >= CHILD_LIFETIME {
        return None;
    }
    let runtime = runtime.clone();
    let scope = config::canonical_directory(&session.cwd).ok()?;
    (runtime.owner.matches(session) && runtime.scope == scope).then_some(runtime)
}

fn runtime_matches_record(record: &TaskRecord) -> bool {
    runtimes()
        .lock()
        .unwrap()
        .get(&record.task_id)
        .is_some_and(|runtime| runtime.owner == record.owner && runtime.scope == record.scope_cwd)
}

async fn insert_runtime(
    session: &config::Session,
    task_id: Uuid,
    store: &TaskStore,
    client: ServeClient,
    lease: Arc<TaskRuntimeLease>,
) -> Result<()> {
    let owner = SessionInstance::from_session(session);
    let _permit = ensure_current_active_instance(&owner, session).await?;
    insert_runtime_unchecked(session, task_id, store, client, &owner, lease)
}

fn insert_runtime_unchecked(
    session: &config::Session,
    task_id: Uuid,
    store: &TaskStore,
    client: ServeClient,
    owner: &SessionInstance,
    lease: Arc<TaskRuntimeLease>,
) -> Result<()> {
    let owner = owner.clone();
    let store = store.clone();
    let scope = config::canonical_directory(&session.cwd)?;
    let runtime = RuntimeHandle {
        client: client.clone(),
        owner: owner.clone(),
        scope,
        started_at: Instant::now(),
        _lease: lease,
    };
    {
        let _guard = store_lock().lock().unwrap();
        let registry = lifecycle_registry().lock().unwrap();
        anyhow::ensure!(
            registry
                .entries
                .get(&owner)
                .is_none_or(|entry| !entry.closing),
            "OpenCode session instance is closing"
        );
        let mut state = runtimes().lock().unwrap();
        anyhow::ensure!(
            !state.contains_key(&task_id),
            "OpenCode task runtime is already registered"
        );
        state.insert(task_id, runtime);
    }
    tokio::spawn(async move {
        let session_stopped = tokio::select! {
            _ = tokio::time::sleep(CHILD_LIFETIME) => false,
            _ = wait_for_session_stop(owner.clone()) => true,
        };
        let client = {
            let _guard = store_lock().lock().unwrap();
            runtimes()
                .lock()
                .unwrap()
                .get(&task_id)
                .filter(|runtime| runtime.owner == owner)
                .map(|runtime| runtime.client.clone())
        };
        if let Some(client) = client {
            client.shutdown().await;
        }
        {
            let _guard = store_lock().lock().unwrap();
            let mut state = runtimes().lock().unwrap();
            if state
                .get(&task_id)
                .is_some_and(|runtime| runtime.owner == owner)
            {
                state.remove(&task_id);
            }
        }
        if session_stopped {
            let result = async {
                wait_for_session_inflight_drain(&owner, SESSION_TASK_DRAIN_TIMEOUT).await?;
                finalize_session_tasks(&owner, &store).await
            }
            .await;
            if let Err(error) = result {
                eprintln!(
                    "failed to finalize OpenCode tasks after session {} stopped: {error:#}",
                    owner.id
                );
            }
        }
    });
    Ok(())
}

async fn wait_for_session_stop(owner: SessionInstance) {
    loop {
        let same_instance_active = match config::read_session_metadata(&owner.id).await {
            Ok(session) if owner.matches(&session) => {
                config::session_is_active(&owner.id).await.unwrap_or(false)
            }
            Ok(_) | Err(_) => false,
        };
        if !same_instance_active {
            begin_session_instance_shutdown(&owner);
            return;
        }
        tokio::time::sleep(SESSION_STOP_POLL).await;
    }
}

async fn active_session_exists(session_id: &str) -> bool {
    if config::read_session_metadata(session_id).await.is_err() {
        // Unknown lifecycle state must not erase evidence that could belong to
        // a replacement session.
        return true;
    }
    config::session_is_active(session_id).await.unwrap_or(true)
}

async fn shutdown_session_runtimes(owner: &SessionInstance) {
    let clients = {
        let _guard = store_lock().lock().unwrap();
        let state = runtimes().lock().unwrap();
        state
            .values()
            .filter_map(|runtime| (runtime.owner == *owner).then_some(runtime.client.clone()))
            .collect::<Vec<_>>()
    };
    for client in clients {
        client.shutdown().await;
    }
    {
        let _guard = store_lock().lock().unwrap();
        runtimes()
            .lock()
            .unwrap()
            .retain(|_, runtime| runtime.owner != *owner);
    }
}

async fn remove_session_evidence(owner: &SessionInstance) {
    if !active_session_exists(&owner.id).await {
        evidence::remove_session(&owner.id);
    }
}

async fn finalize_session_tasks(owner: &SessionInstance, store: &TaskStore) -> Result<()> {
    let deadline = Instant::now() + SESSION_TASK_DRAIN_TIMEOUT;
    loop {
        let outcome = store
            .finalize_owner(owner)
            .context("failed to finalize OpenCode tasks for ended session instance")?;
        if !outcome.deferred {
            let _ = outcome.finalized;
            remove_session_evidence(owner).await;
            finish_session_shutdown(owner)?;
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out waiting for a remotely owned OpenCode task runtime to stop"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub(crate) async fn remove_session(session: &config::Session) -> Result<()> {
    let owner = SessionInstance::from_session(session);
    begin_session_instance_shutdown(&owner);
    shutdown_session_runtimes(&owner).await;
    wait_for_session_inflight_drain(&owner, SESSION_TASK_DRAIN_TIMEOUT).await?;
    match TaskStore::default_store() {
        Ok(store) => finalize_session_tasks(&owner, &store).await,
        Err(error) => {
            remove_session_evidence(&owner).await;
            Err(error).context("cannot open OpenCode task store for session cleanup")
        }
    }
}

// ---------- reconciliation ----------

struct DerivedServeState {
    status: TaskStatus,
    usage: Option<BTreeMap<String, u64>>,
    observed_model: Option<String>,
    report: Option<Value>,
    last_error: Option<String>,
}

fn extract_usage(info: &Value) -> Option<BTreeMap<String, u64>> {
    let tokens = info.get("tokens").or_else(|| info.get("usage"))?;
    let object = tokens.as_object()?;
    let usage: BTreeMap<String, u64> = object
        .iter()
        .filter_map(|(key, value)| value.as_u64().map(|value| (key.clone(), value)))
        .take(16)
        .collect();
    (!usage.is_empty()).then_some(usage)
}

fn message_is_assistant(message: &Value) -> bool {
    // V1 wraps fields in `info`; V2 keeps them top-level with `type`.
    let info = message.get("info").unwrap_or(message);
    info.get("role")
        .or_else(|| info.get("type"))
        .and_then(Value::as_str)
        == Some("assistant")
        || message
            .get("role")
            .or_else(|| message.get("type"))
            .and_then(Value::as_str)
            == Some("assistant")
}

fn message_completed(message: &Value) -> bool {
    let info = message.get("info").unwrap_or(message);
    info.get("time")
        .and_then(|time| time.get("completed"))
        .is_some_and(|value| value.is_number() && value.as_u64().unwrap_or(0) > 0)
}

fn message_error(message: &Value) -> Option<String> {
    let info = message.get("info").unwrap_or(message);
    let error = info.get("error")?;
    let rendered = match error {
        Value::String(text) => text.clone(),
        Value::Object(_) => error
            .get("message")
            .or_else(|| error.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string()),
        _ => error.to_string(),
    };
    Some(bound_text(&rendered, MAX_ERROR_BYTES))
}

fn message_text(message: &Value) -> String {
    // V1 emits `parts`; V2 emits the same typed part list under `content`.
    let parts = message
        .get("parts")
        .or_else(|| message.get("content"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut text = String::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if part.get("synthetic").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(chunk) = part.get("text").and_then(Value::as_str)
            && text.len() < MAX_REPORT_BYTES * 4
        {
            text.push_str(chunk);
        }
    }
    text
}

fn observed_model_from(message: &Value) -> Option<String> {
    let info = message.get("info").unwrap_or(message);
    // V2 keeps a structured `model: {id, providerID}` object instead of the
    // flat providerID/modelID strings.
    let model_ref = info.get("model");
    let provider = info
        .get("providerID")
        .or_else(|| info.get("provider_id"))
        .or_else(|| model_ref.and_then(|model| model.get("providerID")))
        .and_then(Value::as_str);
    let model = info
        .get("modelID")
        .or_else(|| info.get("model_id"))
        .or_else(|| model_ref.and_then(|model| model.get("id")))
        .or(model_ref)
        .and_then(Value::as_str);
    match (provider, model) {
        (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
        (None, Some(model)) => Some(model.to_owned()),
        _ => None,
    }
}

fn derive_serve_state(
    record: &TaskRecord,
    session_id: &str,
    status: &Value,
    messages: &[Value],
    permissions: &[Value],
    questions: &[Value],
) -> DerivedServeState {
    let session_pending = |items: &[Value]| {
        items.iter().any(|item| {
            item.get("sessionID")
                .or_else(|| item.get("session_id"))
                .and_then(Value::as_str)
                == Some(session_id)
        })
    };
    if session_pending(permissions) || session_pending(questions) {
        return DerivedServeState {
            status: TaskStatus::WaitingApproval,
            usage: None,
            observed_model: None,
            report: None,
            last_error: Some("opencode permission/question request is pending".to_owned()),
        };
    }

    let entry = status.get(session_id);
    let busy = entry
        .and_then(|entry| entry.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "busy" | "retry"));

    let assistant = messages
        .iter()
        .rev()
        .find(|message| message_is_assistant(message));
    let usage = assistant.and_then(|message| {
        let info = message.get("info").unwrap_or(message);
        extract_usage(info)
    });
    let observed_model = assistant.and_then(observed_model_from);

    if busy {
        return DerivedServeState {
            status: TaskStatus::Running,
            usage,
            observed_model,
            report: None,
            last_error: None,
        };
    }

    let Some(assistant) = assistant else {
        // Prompt admitted but no assistant turn exists yet. Give the server a
        // short grace window before treating the turn as dropped.
        let now = config::unix_time();
        let status = if now.saturating_sub(record.updated_at) < PROMPT_ADMISSION_GRACE_SECONDS {
            TaskStatus::Running
        } else {
            TaskStatus::RetryableFailed
        };
        return DerivedServeState {
            status,
            usage,
            observed_model,
            report: None,
            last_error: (status == TaskStatus::RetryableFailed)
                .then(|| "opencode turn ended without an assistant reply".to_owned()),
        };
    };

    if let Some(error) = message_error(assistant) {
        return DerivedServeState {
            status: TaskStatus::RetryableFailed,
            usage,
            observed_model,
            report: None,
            last_error: Some(error),
        };
    }

    if !message_completed(assistant) {
        // The turn was orphaned mid-run (e.g. abort raced completion).
        return DerivedServeState {
            status: TaskStatus::RetryableFailed,
            usage,
            observed_model,
            report: None,
            last_error: Some("opencode turn ended without completion".to_owned()),
        };
    }

    let text = message_text(assistant);
    let (report, report_error) = extract_report(&text);
    DerivedServeState {
        status: TaskStatus::Completed,
        usage,
        observed_model,
        report,
        last_error: report_error,
    }
}

fn bound_text(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// Extract the last balanced JSON object from assistant text and validate the
/// bounded delegation report contract.
fn extract_report(text: &str) -> (Option<Value>, Option<String>) {
    let mut best: Option<Value> = None;
    for (index, _) in text.match_indices('{') {
        let Some(value) = parse_balanced_json(&text[index..]) else {
            continue;
        };
        if report_shape_valid(&value) {
            best = Some(value);
        }
    }
    match best {
        Some(report) => (Some(report), None),
        None if text.trim().is_empty() => (None, Some("assistant reply was empty".to_owned())),
        None => (
            None,
            Some("assistant reply did not contain a valid report JSON object".to_owned()),
        ),
    }
}

fn parse_balanced_json(text: &str) -> Option<Value> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return serde_json::from_str(&text[..=offset]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

fn report_shape_valid(report: &Value) -> bool {
    let Some(object) = report.as_object() else {
        return false;
    };
    let status_ok = object
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| {
            matches!(
                status,
                "completed" | "failed" | "blocked" | "needs_decision"
            )
        });
    if !status_ok {
        return false;
    }
    if object
        .get("summary")
        .and_then(Value::as_str)
        .is_none_or(|summary| summary.chars().count() > MAX_SUMMARY_CHARS)
    {
        return false;
    }
    for key in ["changed_files", "checks", "unresolved"] {
        match object.get(key) {
            Some(Value::Array(items))
                if items.len() <= MAX_REPORT_ARRAY_ITEMS
                    && items.iter().all(|item| item.is_string()) => {}
            None => {}
            _ => return false,
        }
    }
    serde_json::to_vec(report)
        .map(|bytes| bytes.len() <= MAX_REPORT_BYTES)
        .unwrap_or(false)
}

async fn reconcile_task(
    session: &config::Session,
    owner: &SessionInstance,
    store: &TaskStore,
    record: TaskRecord,
    client: &ServeClient,
) -> Result<TaskRecord> {
    let Some(opencode_session_id) = record.opencode_session_id.clone() else {
        // Accepted but never bound to an opencode session: the spawn failed
        // before session create, or the crash landed in that window. The task
        // text is never persisted, so this cannot be re-driven safely.
        return store.update_if_instance_live(session, record.task_id, owner, |record| {
            if record.status.is_terminal() {
                return Ok(());
            }
            record.status = TaskStatus::ReconciliationRequired;
            record.revision = record.revision.saturating_add(1);
            let outcome = record.outcome();
            if let Some(receipt) = record
                .operations
                .iter_mut()
                .find(|receipt| receipt.action == "start")
            {
                receipt.phase = OperationPhase::Accepted;
                receipt.outcome = outcome;
            }
            Ok(())
        });
    };

    let status = client.session_status().await.unwrap_or_else(|_| json!({}));
    let messages = client
        .messages(&opencode_session_id, MAX_MESSAGES_SCAN)
        .await
        .unwrap_or_default();
    let permissions = client.permission_list().await.unwrap_or_default();
    let questions = client.question_list().await.unwrap_or_default();

    let derived = derive_serve_state(
        &record,
        &opencode_session_id,
        &status,
        &messages,
        &permissions,
        &questions,
    );

    // Accepted-start admission check: the deterministic prompt message id
    // tells us whether the start prompt ever reached the server. When it did
    // not, the original task text is unavailable to this code path (it is
    // never persisted to the record), so reconcile to the explicit
    // reconciliation_required state for a caller-driven retry rather than
    // replaying blindly.
    if matches!(
        record.status,
        TaskStatus::Accepted | TaskStatus::ReconciliationRequired
    ) && let Some(start_operation_id) = record
        .operations
        .iter()
        .find(|receipt| receipt.action == "start")
        .map(|receipt| receipt.operation_id)
    {
        let message_id = prompt_message_id(start_operation_id);
        let admitted = messages.iter().any(|message| {
            let info = message.get("info").unwrap_or(message);
            info.get("id").and_then(Value::as_str) == Some(message_id.as_str())
        });
        if !admitted {
            return store.update_if_instance_live(session, record.task_id, owner, |record| {
                if record.status.is_terminal() {
                    return Ok(());
                }
                record.status = TaskStatus::ReconciliationRequired;
                record.revision = record.revision.saturating_add(1);
                update_operation_receipt(record, start_operation_id, OperationPhase::Accepted);
                Ok(())
            });
        }
    }

    apply_derived(session, owner, store, record, derived).await
}

async fn apply_derived(
    session: &config::Session,
    owner: &SessionInstance,
    store: &TaskStore,
    record: TaskRecord,
    derived: DerivedServeState,
) -> Result<TaskRecord> {
    store.update_if_instance_live(session, record.task_id, owner, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        if record.status != derived.status {
            record.status = derived.status;
        }
        if derived.usage.is_some() {
            record.usage = derived.usage.clone();
        }
        if derived.observed_model.is_some() {
            record.observed_model = derived.observed_model.clone();
        }
        if derived.report.is_some() {
            record.report = derived.report.clone();
        }
        if derived.last_error.is_some() {
            record.last_error = derived.last_error.clone();
        }
        record.revision = record.revision.saturating_add(1);
        let outcome = record.outcome();
        if derived.status.is_terminal()
            && let Some(receipt) = record.operations.iter_mut().find(|receipt| {
                receipt.action == "start" && receipt.phase == OperationPhase::Accepted
            })
        {
            receipt.phase = OperationPhase::Applied;
            receipt.outcome = outcome;
        }
        Ok(())
    })
}

fn prompt_body(model: Option<&str>, agent: Option<&str>, variant: Option<&str>) -> Value {
    let mut body = json!({});
    if let Some(model) = model
        && let Some((provider, model_id)) = model.split_once('/')
    {
        body["model"] = json!({"providerID": provider, "modelID": model_id});
    }
    if let Some(agent) = agent {
        body["agent"] = json!(agent);
    }
    if let Some(variant) = variant {
        body["variant"] = json!(variant);
    }
    body
}

enum EnsuredRuntime {
    Local(ServeClient),
    OwnedElsewhere,
}

async fn ensure_runtime_with_binary(
    session: &config::Session,
    record: &TaskRecord,
    store: &TaskStore,
    binary: &Path,
    acquired_lease: Option<TaskRuntimeLease>,
) -> Result<EnsuredRuntime> {
    let owner = SessionInstance::from_session(session);
    if let Some(runtime) = runtime_for(session, record.task_id) {
        return Ok(EnsuredRuntime::Local(runtime.client));
    }
    let lease = match acquired_lease {
        Some(lease) => Arc::new(lease),
        None => match store.try_acquire_runtime_lease(record.task_id)? {
            Some(lease) => Arc::new(lease),
            None => return Ok(EnsuredRuntime::OwnedElsewhere),
        },
    };
    let _operation_permit = ensure_current_active_instance(&owner, session).await?;
    let client = spawn_serve(session, record.task_id, store, Arc::clone(&lease), binary).await?;
    insert_runtime(session, record.task_id, store, client.clone(), lease).await?;
    Ok(EnsuredRuntime::Local(client))
}

// ---------- public entry points ----------

pub(crate) async fn status(session: &config::Session) -> Result<Value> {
    let owner = SessionInstance::from_session(session);
    let _permit = ensure_current_active_instance(&owner, session).await?;
    let binary = crate::cli::codex::delegation::opencode::resolve_default_opencode_executable()
        .map_err(|error| anyhow::anyhow!("{}", error.message()))?;
    let store = TaskStore::default_store()?;
    let probe_id = Uuid::new_v4();
    let lease = Arc::new(
        store
            .try_acquire_runtime_lease(probe_id)?
            .context("cannot acquire OpenCode probe lease")?,
    );
    let client = spawn_serve(session, probe_id, &store, lease, binary.binary()).await;
    let result = match client {
        Ok(client) => {
            let health = client.health().await;
            let providers = client.provider_list().await;
            let tail = client.diagnostics_tail();
            client.shutdown().await;
            match (health, providers) {
                (Ok(health), Ok(providers)) => Ok((health, providers, tail)),
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        }
        Err(error) => Err(error),
    };
    cleanup_probe_state(&store, probe_id);
    let (health, providers, tail) = result?;
    let providers = providers
        .get("all")
        .or_else(|| providers.get("providers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let models = providers
        .iter()
        .filter_map(|provider| {
            let id = provider
                .get("id")
                .or_else(|| provider.get("name"))
                .and_then(Value::as_str)?;
            let count = provider
                .get("models")
                .and_then(Value::as_object)
                .map(|models| models.len());
            Some(json!({"provider": id, "models": count}))
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "compatible": true,
        "serve_version": health.get("version"),
        "serve_healthy": health.get("healthy"),
        "binary": binary.binary(),
        "providers": models,
        "diagnostics_tail": tail,
    }))
}

fn cleanup_probe_state(store: &TaskStore, probe_id: Uuid) {
    let _ = std::fs::remove_file(store.runtime_lock_path(probe_id));
    let _ = std::fs::remove_dir_all(store.runtime_state_directory(probe_id));
}

pub(crate) async fn task_start(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_start_with_store_and_binary(args, session, &store, Path::new("opencode")).await
}

async fn task_start_with_store_and_binary(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
) -> Result<Value> {
    let owner = SessionInstance::from_session(session);
    let operation_id = required_uuid(args, "operation_id")?;
    let task = required_string(args, "task")?;
    let model = optional_string(args, "model")?;
    let agent = optional_string(args, "agent")?;
    let variant = optional_string(args, "variant")?;
    validate_task_input(task, "task")?;
    if let Some(model) = model {
        validate_argument(model, "model")?;
        anyhow::ensure!(
            model.contains('/'),
            "model must be a provider/model pair such as anthropic/claude-sonnet-4"
        );
    }
    if let Some(agent) = agent {
        validate_argument(agent, "agent")?;
    }
    if let Some(variant) = variant {
        validate_argument(variant, "variant")?;
    }

    let task_id = task_id_for_operation(session, operation_id)?;
    let request_fingerprint = fingerprint(&json!({
        "kind": "start",
        "task_id": task_id,
        "task": task,
        "model": model,
        "agent": agent,
        "variant": variant,
    }))?;
    let now = config::unix_time();
    let mut record = TaskRecord {
        schema_version: TASK_SCHEMA_VERSION,
        task_id,
        owner: SessionInstance::from_session(session),
        scope_cwd: config::canonical_directory(&session.cwd)?,
        model: model.map(str::to_owned),
        agent: agent.map(str::to_owned),
        variant: variant.map(str::to_owned),
        status: TaskStatus::Accepted,
        revision: 1,
        generation: 0,
        opencode_session_id: None,
        usage: None,
        observed_model: None,
        report: None,
        last_error: None,
        created_at: now,
        updated_at: now,
        operations: Vec::new(),
        operation_tombstones: Vec::new(),
    };
    record.operations.push(OperationReceipt {
        operation_id,
        request_fingerprint,
        action: "start".to_owned(),
        phase: OperationPhase::Accepted,
        outcome: record.outcome(),
    });
    let acceptance_permit = ensure_current_active_instance(&owner, session).await?;
    let acceptance = store.accept_start_if_instance_live(session, record, &owner)?;
    drop(acceptance_permit);
    let (mut record, runtime_lease) = match acceptance {
        StartAcceptance::Existing(existing) => {
            return replay_operation(&existing, operation_id, request_fingerprint);
        }
        StartAcceptance::Accepted(record, lease) => (record, lease),
    };
    let runtime_lease = Arc::new(runtime_lease);
    let _operation_permit = ensure_current_active_instance(&owner, session).await?;

    let client = match spawn_serve(session, task_id, store, Arc::clone(&runtime_lease), binary)
        .await
    {
        Ok(client) => client,
        Err(error) => {
            let shutting_down = session_instance_is_closing(&owner);
            let record = store.update(session, task_id, |record| {
                if !record.status.is_terminal() {
                    record.status = if shutting_down {
                        TaskStatus::Interrupted
                    } else {
                        TaskStatus::RetryableFailed
                    };
                    record.last_error = Some(bound_text(&format!("{error:#}"), MAX_ERROR_BYTES));
                    record.revision = record.revision.saturating_add(1);
                    update_operation_receipt(
                        record,
                        operation_id,
                        if shutting_down {
                            OperationPhase::Applied
                        } else {
                            OperationPhase::RetryableFailed
                        },
                    );
                }
                Ok(())
            })?;
            return Ok(task_view(&record, None));
        }
    };

    let start_result = async {
        let created = client
            .session_create(&json!({
                "title": format!("temote-{task_id}"),
                "agent": agent,
            }))
            .await?;
        let opencode_session_id = created
            .get("id")
            .and_then(Value::as_str)
            .context("opencode session create response is missing id")?
            .to_owned();

        let bind_permit = ensure_current_active_instance(&owner, session).await?;
        record = store.update_if_instance_live(session, task_id, &owner, |record| {
            if record.status.is_terminal() {
                return Ok(());
            }
            anyhow::ensure!(
                record.opencode_session_id.is_none()
                    || record.opencode_session_id.as_deref() == Some(opencode_session_id.as_str()),
                "opencode task session was already bound"
            );
            record.opencode_session_id = Some(opencode_session_id.clone());
            record.revision = record.revision.saturating_add(1);
            Ok(())
        })?;
        drop(bind_permit);

        let mut body = prompt_body(model, agent, variant);
        body["messageID"] = json!(prompt_message_id(operation_id));
        body["parts"] = json!([{"type": "text", "text": format!(
            "{}{}{}",
            REPORT_INSTRUCTIONS
                .replace("__REQUESTED_MODEL__", &serde_json::to_string(&model)?)
                .replace("__REQUESTED_EFFORT__", &serde_json::to_string(&variant)?),
            "\n",
            task
        )}]);
        client.prompt_async(&opencode_session_id, &body).await?;

        let apply_permit = ensure_current_active_instance(&owner, session).await?;
        record = store.update_if_instance_live(session, task_id, &owner, |record| {
            if record.status.is_terminal() {
                return Ok(());
            }
            if matches!(
                record.status,
                TaskStatus::Accepted | TaskStatus::Unknown | TaskStatus::ReconciliationRequired
            ) {
                record.status = TaskStatus::Running;
            }
            record.generation = 1;
            record.revision = record.revision.saturating_add(1);
            update_operation_receipt(record, operation_id, OperationPhase::Applied);
            Ok(())
        })?;
        drop(apply_permit);
        Result::<()>::Ok(())
    }
    .await;

    if let Err(error) = start_result {
        client.shutdown().await;
        let shutting_down = session_instance_is_closing(&owner);
        let record = store.update(session, task_id, |record| {
            if !record.status.is_terminal() {
                record.status = if shutting_down {
                    TaskStatus::Interrupted
                } else {
                    TaskStatus::ReconciliationRequired
                };
                record.last_error = Some(bound_text(&format!("{error:#}"), MAX_ERROR_BYTES));
                record.revision = record.revision.saturating_add(1);
                update_operation_receipt(
                    record,
                    operation_id,
                    if shutting_down {
                        OperationPhase::Applied
                    } else {
                        OperationPhase::Accepted
                    },
                );
            }
            Ok(())
        })?;
        return Ok(task_view(&record, None));
    }

    insert_runtime(session, task_id, store, client.clone(), runtime_lease).await?;
    Ok(task_view(&record, None))
}

fn replay_operation(record: &TaskRecord, operation_id: Uuid, fingerprint: Uuid) -> Result<Value> {
    let Some(receipt) = record
        .operations
        .iter()
        .find(|receipt| receipt.operation_id == operation_id)
    else {
        if let Some(tombstone) = record
            .operation_tombstones
            .iter()
            .find(|tombstone| tombstone.operation_id == operation_id)
        {
            anyhow::ensure!(
                tombstone.request_fingerprint == fingerprint,
                "OPERATION_CONFLICT: operation_id was already accepted with a different request"
            );
            anyhow::bail!(
                "OPERATION_REPLAY_COMPACTED: operation_id was accepted during task retention, but its detailed receipt was compacted; refusing to reapply the mutation"
            );
        }
        anyhow::bail!("OPERATION_CONFLICT: task exists without the requested operation receipt");
    };
    anyhow::ensure!(
        receipt.request_fingerprint == fingerprint,
        "OPERATION_CONFLICT: operation_id was already accepted with a different request"
    );
    if receipt.phase == OperationPhase::Accepted {
        let mut outcome = receipt.outcome.clone();
        outcome.status = TaskStatus::ReconciliationRequired;
        return Ok(operation_view(record.task_id, &outcome));
    }
    Ok(operation_view(record.task_id, &receipt.outcome))
}

pub(crate) async fn task_get(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_get_with_store_and_binary(args, session, &store, Path::new("opencode")).await
}

async fn task_get_with_store_and_binary(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
) -> Result<Value> {
    let task_id = required_uuid(args, "task_id")?;
    let after_revision = optional_u64(args, "after_revision")?;
    let owner = SessionInstance::from_session(session);
    let _load_permit = ensure_current_active_instance(&owner, session).await?;
    let (record, runtime_access) = store.load_for_reconciliation(session, task_id)?;

    if record.status.is_terminal() {
        if after_revision == Some(record.revision) {
            return Ok(json!({
                "task_id": task_id,
                "status": "not_modified",
                "revision": record.revision,
            }));
        }
        return Ok(task_view(&record, None));
    }

    let acquired_lease = match runtime_access {
        RuntimeAccess::Local => None,
        RuntimeAccess::Acquired(lease) => Some(lease),
        RuntimeAccess::OwnedElsewhere => {
            return Ok(task_view_at_revision(&record, after_revision));
        }
    };

    let client =
        match ensure_runtime_with_binary(session, &record, store, binary, acquired_lease).await {
            Ok(EnsuredRuntime::Local(client)) => client,
            Ok(EnsuredRuntime::OwnedElsewhere) => {
                return Ok(task_view_at_revision(&record, after_revision));
            }
            Err(_) => {
                let record = store.update_if_instance_live(session, task_id, &owner, |record| {
                    if !record.status.is_terminal() {
                        record.status = TaskStatus::Unknown;
                        record.revision = record.revision.saturating_add(1);
                    }
                    Ok(())
                })?;
                return Ok(task_view(&record, None));
            }
        };

    let record = reconcile_task(session, &owner, store, record, &client).await;
    let record = match record {
        Ok(record) => record,
        Err(error) => {
            let record = store.update_if_instance_live(session, task_id, &owner, |record| {
                if !record.status.is_terminal() {
                    record.status = TaskStatus::Unknown;
                    record.last_error = Some(bound_text(&format!("{error:#}"), MAX_ERROR_BYTES));
                    record.revision = record.revision.saturating_add(1);
                }
                Ok(())
            })?;
            return Ok(task_view(&record, None));
        }
    };

    // Store bounded evidence for the final assistant turn on terminal states.
    let evidence_ref = if record.status.is_terminal() {
        record
            .opencode_session_id
            .clone()
            .map(|opencode_session_id| {
                json!({
                    "kind": "opencode_task_final_state",
                    "task_id": task_id,
                    "opencode_session_id": opencode_session_id,
                    "status": record.status.as_str(),
                    "report": record.report,
                    "usage": record.usage,
                    "observed_model": record.observed_model,
                })
            })
            .and_then(|payload| store_evidence_for_instance(&owner, session, &payload))
    } else {
        None
    };

    if after_revision == Some(record.revision) {
        return Ok(json!({
            "task_id": task_id,
            "status": "not_modified",
            "revision": record.revision,
        }));
    }
    Ok(task_view(&record, evidence_ref.as_ref()))
}

pub(crate) async fn task_control(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_control_with_store_and_binary(args, session, &store, Path::new("opencode")).await
}

async fn task_control_with_store_and_binary(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
) -> Result<Value> {
    let task_id = required_uuid(args, "task_id")?;
    let operation_id = required_uuid(args, "operation_id")?;
    let action = required_string(args, "action")?;
    anyhow::ensure!(
        matches!(action, "steer" | "resume" | "interrupt"),
        "unsupported OpenCode task action"
    );
    let input = args.get("input").and_then(Value::as_str);
    match action {
        "steer" => validate_task_input(input.context("steer requires input")?, "input")?,
        "resume" | "interrupt" => {
            anyhow::ensure!(input.is_none(), "{action} does not accept input")
        }
        _ => unreachable!(),
    }

    let request_fingerprint = fingerprint(&json!({
        "kind": "control",
        "task_id": task_id,
        "action": action,
        "input": input,
    }))?;
    let owner = SessionInstance::from_session(session);
    let acceptance_permit = ensure_current_active_instance(&owner, session).await?;
    let (mut record, acquired_lease) = match store.accept_control_if_instance_live(
        session,
        task_id,
        operation_id,
        request_fingerprint,
        action,
    )? {
        ControlAcceptance::Replay(result) => return Ok(result),
        ControlAcceptance::Accepted(record, lease) => (*record, lease),
    };
    drop(acceptance_permit);

    let client =
        match ensure_runtime_with_binary(session, &record, store, binary, acquired_lease).await {
            Ok(EnsuredRuntime::Local(client)) => client,
            Ok(EnsuredRuntime::OwnedElsewhere) => {
                let shutting_down = session_instance_is_closing(&owner);
                let record =
                    apply_control_failure(store, session, task_id, operation_id, shutting_down)?;
                return Ok(task_view(&record, None));
            }
            Err(_) => {
                let shutting_down = session_instance_is_closing(&owner);
                let record =
                    apply_control_failure(store, session, task_id, operation_id, shutting_down)?;
                return Ok(task_view(&record, None));
            }
        };

    // For records that never bound a session (crash between accept and
    // create), reconcile first before applying the control action.
    if record.opencode_session_id.is_none() {
        record = match reconcile_task(session, &owner, store, record, &client).await {
            Ok(record) => record,
            Err(_) => {
                let shutting_down = session_instance_is_closing(&owner);
                let record =
                    apply_control_failure(store, session, task_id, operation_id, shutting_down)?;
                return Ok(task_view(&record, None));
            }
        };
    }
    let Some(opencode_session_id) = record.opencode_session_id.clone() else {
        let record = store.update(session, task_id, |record| {
            if !record.status.is_terminal() {
                record.status = TaskStatus::ReconciliationRequired;
                record.revision = record.revision.saturating_add(1);
            }
            Ok(())
        })?;
        return Ok(task_view(&record, None));
    };

    let message_id = prompt_message_id(operation_id);
    let result = match action {
        "steer" => {
            let mut body = prompt_body(
                record.model.as_deref(),
                record.agent.as_deref(),
                record.variant.as_deref(),
            );
            body["messageID"] = json!(message_id);
            body["parts"] = json!([{"type": "text", "text": input.unwrap()}]);
            client.prompt_async(&opencode_session_id, &body).await
        }
        "resume" => {
            let mut body = prompt_body(
                record.model.as_deref(),
                record.agent.as_deref(),
                record.variant.as_deref(),
            );
            body["messageID"] = json!(message_id);
            body["parts"] = json!([{"type": "text", "text": RESUME_INSTRUCTIONS}]);
            client.prompt_async(&opencode_session_id, &body).await
        }
        "interrupt" => client.abort(&opencode_session_id).await,
        _ => unreachable!(),
    };
    if result.is_err() {
        let shutting_down = session_instance_is_closing(&owner);
        let record = apply_control_failure(store, session, task_id, operation_id, shutting_down)?;
        return Ok(task_view(&record, None));
    }

    let apply_permit = ensure_current_active_instance(&owner, session).await?;
    record = store.update_if_instance_live(session, task_id, &owner, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        record.generation = record.generation.saturating_add(1);
        record.status = if action == "interrupt" {
            TaskStatus::Interrupted
        } else {
            TaskStatus::Running
        };
        record.revision = record.revision.saturating_add(1);
        let outcome = record.outcome();
        if let Some(receipt) = record
            .operations
            .iter_mut()
            .find(|receipt| receipt.operation_id == operation_id)
        {
            receipt.phase = OperationPhase::Applied;
            receipt.outcome = outcome;
        }
        Ok(())
    })?;
    drop(apply_permit);

    if action == "interrupt" {
        // Interrupt tears the serve child down: release the runtime so the
        // next get/resume respawns a fresh server with the same task state.
        let removed = {
            let _guard = store_lock().lock().unwrap();
            runtimes().lock().unwrap().remove(&task_id)
        };
        if let Some(runtime) = removed {
            runtime.client.shutdown().await;
        }
    }
    Ok(task_view(&record, None))
}

fn apply_control_failure(
    store: &TaskStore,
    session: &config::Session,
    task_id: Uuid,
    operation_id: Uuid,
    shutting_down: bool,
) -> Result<TaskRecord> {
    store.update(session, task_id, |record| {
        if !record.status.is_terminal() {
            record.status = if shutting_down {
                TaskStatus::Interrupted
            } else {
                TaskStatus::Unknown
            };
            record.revision = record.revision.saturating_add(1);
            update_operation_receipt(
                record,
                operation_id,
                if shutting_down {
                    OperationPhase::Applied
                } else {
                    OperationPhase::RetryableFailed
                },
            );
        }
        Ok(())
    })
}

fn required_uuid(args: &Value, key: &str) -> Result<Uuid> {
    let value = required_string(args, key)?;
    Uuid::parse_str(value).with_context(|| format!("{key} must be a UUID"))
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing or invalid {key}"))
}

fn optional_string<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .with_context(|| format!("{key} must be a string")),
    }
}

fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>> {
    args.get(key)
        .map(|value| {
            value
                .as_u64()
                .with_context(|| format!("{key} must be a non-negative integer"))
        })
        .transpose()
}

// ---------- tests ----------

#[cfg(test)]
type SpawnHook = dyn Fn(&config::Session, Uuid) -> Result<ServeClient> + Send + Sync;

#[cfg(test)]
fn spawn_hook() -> Option<Arc<SpawnHook>> {
    SPAWN_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone()
}

#[cfg(test)]
static SPAWN_HOOK: OnceLock<Mutex<Option<Arc<SpawnHook>>>> = OnceLock::new();

#[cfg(test)]
#[derive(Default)]
struct FakeSession {
    busy: bool,
    messages: Vec<Value>,
}

#[cfg(test)]
#[derive(Default)]
struct FakeServe {
    dead: bool,
    next_session: u64,
    sessions: HashMap<String, FakeSession>,
    pending_permissions: Vec<Value>,
    pending_questions: Vec<Value>,
    prompt_calls: Vec<(String, Value)>,
    abort_calls: Vec<String>,
    create_fail: Option<String>,
    prompt_fail: Option<String>,
    version: String,
    providers: Value,
}

#[cfg(test)]
impl FakeServe {
    fn session_id_of(&mut self) -> String {
        self.next_session += 1;
        format!("ses_{:04}", self.next_session)
    }

    fn require_live(&self) -> Result<()> {
        anyhow::ensure!(!self.dead, "fake serve stopped");
        Ok(())
    }

    fn health(&mut self) -> Result<Value> {
        self.require_live()?;
        Ok(json!({"healthy": true, "version": self.version}))
    }

    fn provider_list(&mut self) -> Result<Value> {
        self.require_live()?;
        Ok(self.providers.clone())
    }

    fn session_create(&mut self, _body: &Value) -> Result<Value> {
        self.require_live()?;
        if let Some(error) = &self.create_fail {
            anyhow::bail!("{error}");
        }
        let id = self.session_id_of();
        self.sessions.insert(
            id.clone(),
            FakeSession {
                busy: false,
                messages: Vec::new(),
            },
        );
        Ok(json!({"id": id}))
    }

    fn prompt_async(&mut self, session_id: &str, body: &Value) -> Result<()> {
        self.require_live()?;
        if let Some(error) = &self.prompt_fail {
            anyhow::bail!("{error}");
        }
        self.prompt_calls
            .push((session_id.to_owned(), body.clone()));
        let session = self
            .sessions
            .get_mut(session_id)
            .with_context(|| format!("unknown opencode session {session_id}"))?;
        session.busy = true;
        let message_id = body
            .get("messageID")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let text = body
            .get("parts")
            .and_then(Value::as_array)
            .and_then(|parts| parts.first())
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        session.messages.push(json!({
            "info": {"id": message_id, "role": "user", "time": {"created": 50}},
            "parts": [{"type": "text", "text": text}],
        }));
        Ok(())
    }

    fn abort(&mut self, session_id: &str) -> Result<()> {
        self.require_live()?;
        self.abort_calls.push(session_id.to_owned());
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.busy = false;
        }
        Ok(())
    }

    fn session_status(&mut self) -> Result<Value> {
        self.require_live()?;
        let status = self
            .sessions
            .iter()
            .map(|(id, session)| {
                (
                    id.clone(),
                    json!({"type": if session.busy { "busy" } else { "idle" }}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        Ok(Value::Object(status))
    }

    fn messages(&mut self, session_id: &str, limit: u32) -> Result<Vec<Value>> {
        self.require_live()?;
        let session = self
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown opencode session {session_id}"))?;
        let start = session.messages.len().saturating_sub(limit as usize);
        Ok(session.messages[start..].to_vec())
    }

    fn permission_list(&mut self) -> Result<Vec<Value>> {
        self.require_live()?;
        Ok(self.pending_permissions.clone())
    }

    fn question_list(&mut self) -> Result<Vec<Value>> {
        self.require_live()?;
        Ok(self.pending_questions.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals;

    /// Serialize tests that install the shared SPAWN_HOOK; the fake is global
    /// state and parallel installation would cross-drive other tests.
    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    async fn active_test_session(
        root: &Path,
        id: &str,
        yolo: bool,
    ) -> (approvals::RuntimeHandle, config::Session) {
        let (approval_sender, _approval_receiver) = approvals::approval_channel();
        let handle = approvals::spawn_runtime(root, Some(id), yolo, approval_sender)
            .await
            .unwrap();
        let session = config::read_session_metadata(id).await.unwrap();
        (handle, session)
    }

    fn test_id() -> String {
        format!("opencode-test-{}", Uuid::new_v4().simple())
    }

    fn fake_client() -> (ServeClient, Arc<Mutex<FakeServe>>) {
        let fake = Arc::new(Mutex::new(FakeServe {
            version: "9.9.9-test".to_owned(),
            providers: json!({
                "all": [
                    {"id": "anthropic", "models": {"claude-sonnet-4": {}}},
                    {"id": "openai", "models": {"gpt-5.6": {}}},
                ]
            }),
            ..FakeServe::default()
        }));
        (ServeClient::Fake(Arc::clone(&fake)), fake)
    }

    fn complete_turn(
        fake: &Arc<Mutex<FakeServe>>,
        session_id: &str,
        report: &Value,
        tokens: Option<Value>,
    ) {
        let mut fake = fake.lock().unwrap();
        let session = fake.sessions.get_mut(session_id).unwrap();
        session.busy = false;
        let mut info = json!({
            "id": Uuid::new_v4().simple().to_string(),
            "role": "assistant",
            "providerID": "anthropic",
            "modelID": "claude-sonnet-4",
            "time": {"created": 100, "completed": 200},
        });
        if let Some(tokens) = tokens {
            info["tokens"] = tokens;
        }
        session.messages.push(json!({
            "info": info,
            "parts": [{"type": "text", "text": report.to_string()}],
        }));
    }

    fn install_fake(fake: Arc<Mutex<FakeServe>>) {
        let hook: Arc<SpawnHook> = Arc::new(move |_, _| Ok(ServeClient::Fake(Arc::clone(&fake))));
        *SPAWN_HOOK.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(hook);
    }

    fn clear_fake() {
        *SPAWN_HOOK.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("opencode-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_store(root: &Path) -> TaskStore {
        TaskStore::new(root.join("opencode-tasks"))
    }

    fn start_args(operation_id: Uuid, task: &str) -> Value {
        json!({
            "operation_id": operation_id,
            "task": task,
            "model": "anthropic/claude-sonnet-4",
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_persists_acceptance_and_drives_serve() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (client, fake) = fake_client();
        install_fake(fake.clone());

        let op = Uuid::new_v4();
        let out = task_start_with_store_and_binary(
            &start_args(op, "implement the feature"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        clear_fake();

        assert_eq!(out["status"], "running");
        let task_id = Uuid::parse_str(out["task_id"].as_str().unwrap()).unwrap();
        assert_eq!(task_id, task_id_for_operation(&session, op).unwrap());
        let fake = fake.lock().unwrap();
        assert_eq!(fake.prompt_calls.len(), 1);
        let (session_id, body) = &fake.prompt_calls[0];
        assert!(fake.sessions.contains_key(session_id));
        assert_eq!(body["messageID"].as_str().unwrap(), prompt_message_id(op));
        assert!(
            body["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("implement the feature")
        );
        let _ = client;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_replays_same_operation_id_without_respawning() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client();
        install_fake(fake.clone());

        let op = Uuid::new_v4();
        let first = task_start_with_store_and_binary(
            &start_args(op, "same task"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        let second = task_start_with_store_and_binary(
            &start_args(op, "same task"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        clear_fake();
        assert_eq!(first["task_id"], second["task_id"]);
        assert_eq!(first["status"], second["status"]);
        assert_eq!(fake.lock().unwrap().prompt_calls.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_conflicting_fingerprint_errors() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client();
        install_fake(fake);

        let op = Uuid::new_v4();
        task_start_with_store_and_binary(
            &start_args(op, "original task"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        let error = task_start_with_store_and_binary(
            &start_args(op, "different task"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap_err();
        clear_fake();
        assert!(format!("{error:#}").contains("OPERATION_CONFLICT"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_completes_from_assistant_message() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client();
        install_fake(fake.clone());

        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "do the thing"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        let task_id = out["task_id"].as_str().unwrap().to_owned();
        let session_id = {
            let fake = fake.lock().unwrap();
            fake.sessions.keys().next().unwrap().clone()
        };
        let report = json!({
            "status": "completed",
            "summary": "done",
            "base_commit": "",
            "changed_files": ["src/lib.rs"],
            "checks": ["cargo test"],
            "unresolved": [],
            "requested_model": "anthropic/claude-sonnet-4",
            "requested_effort": null,
            "observed_model": "anthropic/claude-sonnet-4",
            "observed_effort": null,
        });
        complete_turn(
            &fake,
            &session_id,
            &report,
            Some(json!({"inputTokens": 10, "outputTokens": 5, "totalTokens": 15})),
        );

        let out = task_get_with_store_and_binary(
            &json!({"task_id": task_id}),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        clear_fake();

        assert_eq!(out["status"], "completed");
        assert_eq!(out["report"]["summary"], "done");
        assert_eq!(out["usage"]["totalTokens"], 15);
        assert_eq!(out["observed_model"], "anthropic/claude-sonnet-4");
        assert!(out["evidence"].is_object());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_deferred_when_lease_held_elsewhere() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client();
        install_fake(fake.clone());

        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "work"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        let task_id = Uuid::parse_str(out["task_id"].as_str().unwrap()).unwrap();
        // Drop the in-process runtime so the lease must be re-acquired, then
        // hold the runtime lock on a second fd as a foreign owner would.
        runtimes().lock().unwrap().remove(&task_id);
        let lock_path = store.runtime_lock_path(task_id);
        let foreign = open_existing_private_lock_file(&lock_path).unwrap();
        let held = unsafe { libc::flock(foreign.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        assert!(held);

        let out = task_get_with_store_and_binary(
            &json!({"task_id": task_id}),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        clear_fake();
        unsafe {
            libc::flock(foreign.as_raw_fd(), libc::LOCK_UN);
        }
        assert_eq!(out["reconciliation_deferred"], true);
        assert_eq!(out["status"], "running");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_marks_unbound_record_reconciliation_required() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client();
        install_fake(fake.clone());

        // Write an accepted-but-never-bound record directly.
        let op = Uuid::new_v4();
        let task_id = task_id_for_operation(&session, op).unwrap();
        let mut record = record_for(&session, task_id, TaskStatus::Accepted, None);
        record.operations.push(OperationReceipt {
            operation_id: op,
            request_fingerprint: Uuid::new_v4(),
            action: "start".to_owned(),
            phase: OperationPhase::Accepted,
            outcome: record.outcome(),
        });
        store.accept_start(&session, record).unwrap();

        let out = task_get_with_store_and_binary(
            &json!({"task_id": task_id}),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        clear_fake();
        assert_eq!(out["status"], "reconciliation_required");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn control_steer_queues_prompt_and_bumps_generation() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client();
        install_fake(fake.clone());

        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "task"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        let task_id = out["task_id"].as_str().unwrap().to_owned();

        let out = task_control_with_store_and_binary(
            &json!({
                "task_id": task_id,
                "operation_id": Uuid::new_v4(),
                "action": "steer",
                "input": "also fix the typo",
            }),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        clear_fake();
        assert_eq!(out["status"], "running");
        let fake = fake.lock().unwrap();
        assert_eq!(fake.prompt_calls.len(), 2);
        assert_eq!(
            fake.prompt_calls[1].1["parts"][0]["text"].as_str().unwrap(),
            "also fix the typo"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn control_interrupt_aborts_and_finalizes() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client();
        install_fake(fake.clone());

        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "task"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        let task_id = out["task_id"].as_str().unwrap().to_owned();

        let out = task_control_with_store_and_binary(
            &json!({
                "task_id": task_id,
                "operation_id": Uuid::new_v4(),
                "action": "interrupt",
            }),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        clear_fake();
        assert_eq!(out["status"], "interrupted");
        let fake = fake.lock().unwrap();
        assert_eq!(fake.abort_calls.len(), 1);
        assert!(fake.dead);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn control_resume_rejects_running_task() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client();
        install_fake(fake.clone());
        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "task"),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        let task_id = out["task_id"].as_str().unwrap().to_owned();
        let error = task_control_with_store_and_binary(
            &json!({
                "task_id": task_id,
                "operation_id": Uuid::new_v4(),
                "action": "resume",
            }),
            &session,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap_err();
        clear_fake();
        assert!(format!("{error:#}").contains("does not require resume"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_foreign_session_denied() {
        let root = tempdir();
        let (_ha, session_a) = active_test_session(&root, &test_id(), false).await;
        let (_hb, session_b) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client();
        install_fake(fake);
        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "task"),
            &session_a,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap();
        let error = task_get_with_store_and_binary(
            &json!({"task_id": out["task_id"]}),
            &session_b,
            &store,
            Path::new("opencode"),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("OPENCODE_TASK_NOT_FOUND"));
    }

    #[test]
    fn extract_report_accepts_embedded_json() {
        let text = "some intro\n".to_owned()
            + &json!({
                "status": "completed",
                "summary": "s",
                "changed_files": [],
                "checks": [],
                "unresolved": [],
                "requested_model": "m",
                "requested_effort": "e",
                "observed_model": "om",
                "observed_effort": "oe",
            })
            .to_string();
        let (report, error) = extract_report(&text);
        assert!(error.is_none());
        assert_eq!(report.unwrap()["summary"], "s");
    }

    #[test]
    fn extract_report_rejects_non_json_text() {
        let (_report, error) = extract_report("no report here");
        assert!(error.is_some());
    }

    #[test]
    fn fingerprint_is_request_deterministic() {
        let args = json!({"kind": "start", "task_id": "t", "task": "x"});
        assert_eq!(fingerprint(&args).unwrap(), fingerprint(&args).unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn store_prunes_expired_terminal_tasks() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let task_id = Uuid::new_v4();
        let mut record = record_for(&session, task_id, TaskStatus::Completed, Some("ses_x"));
        record.updated_at = record
            .updated_at
            .saturating_sub(TASK_RETENTION_SECONDS + 10);
        {
            let _guard = store.lock().unwrap();
            store.save_locked(&record).unwrap();
            std::fs::create_dir_all(store.runtime_state_directory(task_id)).unwrap();
            let current = record_for(&session, Uuid::new_v4(), TaskStatus::Running, None);
            store.prune_locked(&current).unwrap();
        }
        assert!(!store.path(task_id).exists());
        assert!(!store.runtime_state_directory(task_id).exists());
        assert!(!store.runtime_lock_path(task_id).exists());
    }

    fn find_opencode_binary() -> Option<PathBuf> {
        std::env::var_os("TEMOTE_OPENCODE_BIN")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .or_else(|| {
                std::env::var_os("PATH").and_then(|paths| {
                    std::env::split_paths(&paths).find_map(|dir| {
                        let candidate = dir.join("opencode");
                        candidate.is_file().then_some(candidate)
                    })
                })
            })
    }

    /// Live contract check against a real `opencode serve`: when the env
    /// override forces V2 the probe must land on the `api/*` client and every
    /// transport call the task machinery uses must answer. Skips when no
    /// opencode binary is installed.
    #[tokio::test(flavor = "current_thread")]
    async fn serve_v2_contract_end_to_end() {
        let _serial = serial().await;
        let Some(binary) = find_opencode_binary() else {
            eprintln!("opencode binary not found; skipping live V2 serve test");
            return;
        };
        let root = tempdir();
        let scope = root.join("scope");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&scope).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        let config =
            crate::local_agent::opencode_config(crate::local_agent::Access::WorkspaceWrite, &[])
                .unwrap();
        let config_content = serde_json::to_string(&config).unwrap();

        let serve_password = Uuid::new_v4().simple().to_string();
        let client = spawn_serve_once(
            &binary,
            &scope,
            &data_dir,
            reserve_loopback_port().unwrap(),
            &serve_password,
            &config_content,
            ServeContract::V2,
        )
        .await;
        let client = match client {
            Ok(client) => client,
            Err(error) => panic!("v2-forced opencode serve did not start: {error:#}"),
        };
        let ServeClient::SdkV2(_) = &client else {
            panic!("TEMOTE_OPENCODE_SERVE_CONTRACT=v2 must select the V2 client");
        };

        let health = client.health().await.unwrap();
        assert_eq!(health["healthy"], true);
        assert_eq!(health["contract"], "v2");

        let providers = client.provider_list().await.unwrap();
        assert!(
            providers["all"].is_array(),
            "provider list shape: {providers}"
        );

        let created = client
            .session_create(&json!({"title": "v2-wire", "agent": "build"}))
            .await
            .unwrap();
        let session_id = created["id"].as_str().unwrap().to_owned();

        let message_id = prompt_message_id(Uuid::new_v4());
        client
            .prompt_async(
                &session_id,
                &json!({
                    "messageID": message_id,
                    "parts": [{"type": "text", "text": "say hi"}],
                }),
            )
            .await
            .expect("v2 prompt admission failed");

        // The deterministic prompt id must be echoed as a message so the
        // reconcile admission check can see it.
        let mut admitted = false;
        let mut assistant_done = false;
        for _ in 0..40 {
            let messages = client.messages(&session_id, 16).await.unwrap();
            admitted |= messages.iter().any(|message| {
                message
                    .get("info")
                    .unwrap_or(message)
                    .get("id")
                    .and_then(Value::as_str)
                    == Some(message_id.as_str())
            });
            assistant_done |= messages
                .iter()
                .any(|message| message_is_assistant(message) && message_completed(message));
            if admitted && assistant_done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let messages = client.messages(&session_id, 16).await.unwrap();
        assert!(
            admitted,
            "prompt id must appear in the message list: {messages:?}"
        );
        let assistant = messages
            .iter()
            .find(|message| message_is_assistant(message))
            .expect("assistant message missing");
        assert!(
            !message_text(assistant).is_empty(),
            "assistant text must be extracted from v2 content parts: {assistant}"
        );
        assert!(
            observed_model_from(assistant).is_some(),
            "observed model must parse from v2 model object: {assistant}"
        );

        let status = client.session_status().await.unwrap();
        assert!(status.is_object(), "session status shape: {status}");

        let permissions = client.permission_list().await.unwrap();
        assert!(permissions.iter().all(Value::is_object));
        let questions = client.question_list().await.unwrap();
        assert!(questions.iter().all(Value::is_object));

        let _ = client.abort(&session_id).await;
        client.shutdown().await;

        // Auto detection on a server that answers both contracts must pick a
        // live SDK-backed client (either adapter is correct there).
        let serve_password = Uuid::new_v4().simple().to_string();
        let client = spawn_serve_once(
            &binary,
            &scope,
            &data_dir,
            reserve_loopback_port().unwrap(),
            &serve_password,
            &config_content,
            ServeContract::Auto,
        )
        .await
        .unwrap();
        assert!(
            !matches!(client, ServeClient::Fake(_)),
            "auto probe must select an SDK-backed client"
        );
        assert_eq!(client.health().await.unwrap()["healthy"], true);
        client.shutdown().await;
    }

    fn record_for(
        owner_session: &config::Session,
        task_id: Uuid,
        status: TaskStatus,
        opencode_session_id: Option<&str>,
    ) -> TaskRecord {
        let now = config::unix_time();
        TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(owner_session),
            scope_cwd: owner_session.cwd.clone(),
            model: Some("anthropic/claude-sonnet-4".to_owned()),
            agent: None,
            variant: None,
            status,
            revision: 1,
            generation: u64::from(opencode_session_id.is_some()),
            opencode_session_id: opencode_session_id.map(str::to_owned),
            usage: None,
            observed_model: None,
            report: None,
            last_error: None,
            created_at: now,
            updated_at: now,
            operations: Vec::new(),
            operation_tombstones: Vec::new(),
        }
    }
}
