//! Devin `acp` task backend.
//!
//! Mirrors the Codex app-server / OpenCode serve task contract over a per-task
//! `devin acp` child process speaking the Agent Client Protocol (JSON-RPC over
//! stdio): durable scope-bound task records with idempotent operation receipts,
//! runtime leases that fence one acp owner per task across Temote processes,
//! bounded scoped evidence, and session-owner lifecycle fencing.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
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
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{approvals, config, evidence};

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
const MAX_ASSISTANT_TEXT_BYTES: usize = MAX_REPORT_BYTES * 4;
const MAX_RPC_LINE_BYTES: usize = 4 * 1024 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const ACP_PROTOCOL_VERSION: u64 = 1;
const ACP_CLIENT_NAME: &str = "temote-mcp";
const ACP_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const ACP_TAIL_BYTES: usize = 8 * 1024;
const ACP_SPAWN_ATTEMPTS: usize = 2;
const MAX_ACP_BINARY_PATH_BYTES: usize = 4096;
const CHILD_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const SESSION_STOP_POLL: Duration = Duration::from_secs(1);
const SESSION_TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

const TASK_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x3d, 0x8a, 0x4c, 0x12, 0x9e, 0x57, 0x4b, 0xa0, 0xc6, 0x31, 0x7f, 0x28, 0x9b, 0x45, 0x60, 0xe2,
]);
const REQUEST_FINGERPRINT_NAMESPACE: Uuid = Uuid::from_bytes([
    0x86, 0x41, 0xc7, 0x92, 0x0a, 0x3f, 0x4d, 0x95, 0x7e, 0x18, 0xb3, 0x6d, 0xa4, 0x27, 0x59, 0xf1,
]);

const ACP_CHILD_ENV_ALLOWLIST: &[&str] = &[
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
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "DEVIN_API_KEY",
    "DEVIN_TOKEN",
    "WINDSURF_API_KEY",
    "WINDSURF_TOKEN",
];

const REPORT_INSTRUCTIONS: &str = r#"You are a delegated implementation worker running non-interactively. You must not ask interactive questions, and you must finish the task below before answering.

When finished, respond with ONLY one JSON object and nothing else: no markdown, no code fences, no text before or after the JSON.
The JSON object must contain exactly these fields:
{"status":"completed|failed|blocked|needs_decision","summary":"short summary, at most 1200 characters","base_commit":"","changed_files":[],"checks":[],"unresolved":[],"requested_model":__REQUESTED_MODEL__,"requested_effort":null,"observed_model":null,"observed_effort":null}
Rules:
- All string values are plain strings; changed_files, checks, and unresolved are arrays of strings (use [] when empty).
- Set "requested_model" to __REQUESTED_MODEL__.
- Set "observed_model"/"observed_effort" only when you can actually observe them; otherwise keep null.
- Do not include any other fields.

Task:
"#;

const RESUME_INSTRUCTIONS: &str = "Continue the task. When finished, respond with ONLY the required JSON report object and nothing else.";

// ---------- session-instance lifecycle fencing ----------

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
    anyhow::ensure!(entry.closing, "Devin session instance is not closing");
    anyhow::ensure!(
        entry.in_flight == 0,
        "Devin session shutdown finished with {} in-flight operation(s)",
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
    cancellation: watch::Receiver<bool>,
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
                    "cannot drain Devin operations for a session instance that is not closing"
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
                "Devin session shutdown timed out waiting for in-flight operations to drain (session {})",
                owner.id
            )
        })??;
    Ok(())
}

pub(crate) fn ensure_session_replacement_allowed(session_id: &str) -> Result<()> {
    let registry = lifecycle_registry().lock().unwrap();
    anyhow::ensure!(
        !registry.entries.keys().any(|owner| owner.id == session_id),
        "Devin session {session_id} is still draining its previous instance"
    );
    Ok(())
}

async fn ensure_current_active_instance(
    owner: &SessionInstance,
    session: &config::Session,
) -> Result<LifecyclePermit> {
    anyhow::ensure!(
        owner.matches(session),
        "Devin operation session snapshot does not match its owner instance"
    );
    anyhow::ensure!(
        !session_instance_is_closing(owner),
        "Devin session instance is closing"
    );
    let current = config::read_session_metadata(&owner.id)
        .await
        .with_context(|| format!("cannot verify current Devin session instance {}", owner.id))?;
    anyhow::ensure!(
        owner.matches(&current),
        "Devin session instance is no longer current"
    );
    anyhow::ensure!(
        config::session_is_active(&owner.id).await?,
        "Devin session instance is not active"
    );

    let mut registry = lifecycle_registry().lock().unwrap();
    let entry = registry
        .entries
        .entry(owner.clone())
        .or_insert_with(LifecycleEntry::new);
    anyhow::ensure!(
        !entry.closing,
        "Devin session instance began closing while it was being verified"
    );
    entry.in_flight += 1;
    Ok(LifecyclePermit {
        owner: owner.clone(),
        cancellation: entry.cancellation.subscribe(),
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

// ---------- durable task contract ----------

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
    acp_session_id: Option<String>,
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
    #[serde(default)]
    cloud: bool,
    status: TaskStatus,
    revision: u64,
    generation: u64,
    acp_session_id: Option<String>,
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
            acp_session_id: self.acp_session_id.clone(),
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
            directory: config::state_dir()?.join("devin-acp-tasks"),
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
            Err(error) => Err(error).context("cannot inspect Devin task store"),
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
                    .context("cannot lock Devin task store");
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
                return Err(error).context("cannot lock Devin task runtime");
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
            Err(error).context("cannot inspect Devin task runtime lock")
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
                anyhow::anyhow!("DEVIN_TASK_NOT_FOUND: task was not found")
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
            "Devin task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
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
            .context("Devin session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "Devin session instance is closing");
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
            .context("Devin session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "Devin session instance is closing");
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
            .context("Devin start acceptance is missing its operation receipt")?;
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
                            && existing.acp_session_id.is_none()
                    });
                if retryable {
                    let lease = self
                        .try_acquire_runtime_lease_locked(existing.task_id)?
                        .context(
                            "DEVIN_TASK_RUNTIME_OWNED: task runtime belongs to another Temote process",
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
                        "DEVIN_TASK_RUNTIME_OWNED: task runtime belongs to another Temote process",
                    )?;
                if let Err(error) = self.save_locked(&record) {
                    drop(lease);
                    match std::fs::remove_file(self.runtime_lock_path(record.task_id)) {
                        Ok(()) => {}
                        Err(cleanup_error)
                            if cleanup_error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(cleanup_error) => {
                            return Err(error).context(format!(
                                "cannot remove unused Devin runtime lock: {cleanup_error}"
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
            .context("Devin session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "Devin session instance is closing");
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
                "Devin task does not require resume reconciliation"
            );
        } else {
            anyhow::ensure!(
                matches!(
                    record.status,
                    TaskStatus::Running | TaskStatus::WaitingApproval
                ),
                "Devin task is not active and cannot be controlled"
            );
        }
        anyhow::ensure!(
            record.acp_session_id.is_some() || action == "resume",
            "Devin task requires reconciliation before control"
        );

        let runtime_lease = if runtime_matches_record(&record) {
            None
        } else {
            Some(self.try_acquire_runtime_lease_locked(task_id)?.context(
                "DEVIN_TASK_RUNTIME_OWNED: task runtime belongs to another Temote process",
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
            "Devin task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_TASK_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= MAX_TASK_RECORD_BYTES,
            "Devin task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let record: TaskRecord =
            serde_json::from_slice(&bytes).context("invalid Devin task record")?;
        validate_record(&record)?;
        anyhow::ensure!(record.task_id == task_id, "Devin task record ID mismatch");
        Ok(record)
    }

    fn prune_locked(&self, current: &TaskRecord) -> Result<()> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("cannot list Devin task store"),
        };
        let now = config::unix_time();
        let mut scoped = Vec::new();
        let mut count = 0usize;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "Devin task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
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
                    .with_context(|| format!("cannot prune expired Devin task {id}"))?;
                match std::fs::remove_file(self.runtime_lock_path(id)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("cannot prune Devin runtime lock {id}"));
                    }
                }
                let state_dir = self.runtime_state_directory(id);
                if state_dir.exists()
                    && let Err(error) = std::fs::remove_dir_all(&state_dir)
                {
                    return Err(error)
                        .with_context(|| format!("cannot prune Devin runtime state {id}"));
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
                "Devin task scope has reached its retention limit; refusing to accept another task"
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
            Err(error) => return Err(error).context("cannot inspect Devin task store"),
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
            Err(error) => return Err(error).context("cannot list Devin task store"),
        };
        let now = config::unix_time();
        let mut count = 0usize;
        let mut finalized = 0usize;
        let mut deferred = false;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "Devin task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
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
                    return Err(error)
                        .with_context(|| format!("cannot inspect Devin task {id} during cleanup"));
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
        "Devin task store must be a real directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "Devin task store must be owner-only (mode {mode:04o})"
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
        "Devin task path is not a regular file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(mode & 0o077 == 0, "Devin task file must be owner-only");
    }
    Ok(())
}

fn reject_symlink_target(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "Devin task path may not be a symlink"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect Devin task path"),
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
        "DEVIN_TASK_NOT_FOUND: task was not found"
    );
    Ok(())
}

fn validate_record(record: &TaskRecord) -> Result<()> {
    anyhow::ensure!(
        record.schema_version == TASK_SCHEMA_VERSION,
        "unsupported Devin task schema version"
    );
    config::validate_session_id(&record.owner.id)?;
    let canonical = config::canonical_directory(&record.scope_cwd)?;
    anyhow::ensure!(
        canonical == record.scope_cwd,
        "Devin task scope is not canonical"
    );
    if let Some(model) = &record.model {
        validate_argument(model, "model")?;
    }
    if let Some(agent) = &record.agent {
        validate_argument(agent, "agent")?;
    }
    if let Some(error) = &record.last_error {
        anyhow::ensure!(
            error.len() <= MAX_ERROR_BYTES,
            "Devin task error field exceeds {MAX_ERROR_BYTES} bytes"
        );
    }
    if let Some(report) = &record.report {
        let bytes = serde_json::to_vec(report)?;
        anyhow::ensure!(
            bytes.len() <= MAX_REPORT_BYTES,
            "Devin task report exceeds {MAX_REPORT_BYTES} bytes"
        );
    }
    anyhow::ensure!(record.revision > 0, "Devin task revision must be positive");
    anyhow::ensure!(
        record.operations.len() <= MAX_OPERATION_HISTORY,
        "Devin task operation history exceeds limit"
    );
    let mut operation_ids = record
        .operations
        .iter()
        .map(|receipt| receipt.operation_id)
        .collect::<BTreeSet<_>>();
    for tombstone in &record.operation_tombstones {
        anyhow::ensure!(
            operation_ids.insert(tombstone.operation_id),
            "Devin task operation history contains a duplicate operation_id"
        );
    }
    if let Some(usage) = &record.usage {
        anyhow::ensure!(
            usage.len() <= 16 && usage.values().all(|value| *value <= u64::MAX / 2),
            "Devin task usage has unsupported fields"
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
        "acp_session_id": outcome.acp_session_id,
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
        "cloud": record.cloud,
        "acp_session_id": record.acp_session_id,
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

// ---------- devin acp transport ----------

/// Transport seam over `devin acp` stdio JSON-RPC. The real implementation
/// spawns the ACP child process; tests use an in-memory fake so no binary is
/// required.
#[derive(Clone)]
enum AcpClient {
    Stdio(Arc<StdioAcp>),
    #[cfg(test)]
    Fake(Arc<Mutex<FakeAcp>>),
}

/// Accumulated per-session turn state kept in sync by the stdio actor (or the
/// test fake). `session/prompt` is a long-running JSON-RPC request: its
/// response carries the turn's `stopReason`.
#[derive(Default)]
struct AcpShared {
    session_id: Option<String>,
    session_loaded: bool,
    prompts_in_flight: u32,
    last_stop_reason: Option<String>,
    last_prompt_error: Option<String>,
    terminal_error: Option<String>,
    assistant_text: String,
    last_message_id: Option<String>,
    usage: Option<BTreeMap<String, u64>>,
    pending_permissions: u32,
    observed_model: Option<String>,
}

struct StdioAcp {
    tx: mpsc::Sender<ClientCommand>,
    actor: Arc<Mutex<Option<JoinHandle<()>>>>,
    state: Arc<Mutex<AcpShared>>,
    capabilities: Arc<Mutex<AcpCapabilities>>,
    tail: Arc<Mutex<String>>,
}

#[derive(Clone, Default)]
struct AcpCapabilities {
    load_session: bool,
    session_capabilities: Option<Value>,
    prompt_capabilities: Option<Value>,
    agent_info: Option<Value>,
    protocol_version: Option<u64>,
    auth_methods: usize,
}

enum ClientCommand {
    Request {
        method: &'static str,
        params: Value,
        reply: oneshot::Sender<std::result::Result<Value, String>>,
    },
    Prompt {
        session_id: String,
        text: String,
        admitted: oneshot::Sender<std::result::Result<(), String>>,
        reply: oneshot::Sender<std::result::Result<Value, String>>,
    },
    Notify {
        method: &'static str,
        params: Value,
    },
    Shutdown,
}

struct ServerResponse {
    id: Value,
    payload: std::result::Result<Value, (i64, String)>,
}

/// Task-scoped bindings the client needs to persist waiting-approval state on
/// the task record while a permission request is outstanding.
struct TaskBinding {
    session: config::Session,
    task_id: Uuid,
    owner: SessionInstance,
    store: TaskStore,
}

impl AcpClient {
    async fn request(&self, method: &'static str, params: Value) -> Result<Value> {
        match self {
            Self::Stdio(inner) => {
                let (reply_tx, reply_rx) = oneshot::channel();
                inner
                    .tx
                    .send(ClientCommand::Request {
                        method,
                        params,
                        reply: reply_tx,
                    })
                    .await
                    .context("Devin ACP actor stopped")?;
                let result = tokio::time::timeout(RPC_TIMEOUT, reply_rx)
                    .await
                    .with_context(|| format!("Devin ACP request timed out: {method}"))?
                    .context("Devin ACP actor dropped request")?;
                result.map_err(anyhow::Error::msg)
            }
            #[cfg(test)]
            Self::Fake(fake) => fake.lock().unwrap().request(method, params),
        }
    }

    async fn notify(&self, method: &'static str, params: Value) -> Result<()> {
        match self {
            Self::Stdio(inner) => inner
                .tx
                .send(ClientCommand::Notify { method, params })
                .await
                .context("Devin ACP actor stopped"),
            #[cfg(test)]
            Self::Fake(fake) => fake.lock().unwrap().notify(method, params),
        }
    }

    /// Send `session/prompt`. Returns once the prompt has been written to the
    /// child (admitted); the receiver resolves when the turn ends with a
    /// `stopReason` result or an error.
    async fn session_prompt(
        &self,
        session_id: &str,
        text: String,
    ) -> Result<oneshot::Receiver<std::result::Result<Value, String>>> {
        match self {
            Self::Stdio(inner) => {
                let (admitted_tx, admitted_rx) = oneshot::channel();
                let (reply_tx, reply_rx) = oneshot::channel();
                {
                    let mut state = inner.state.lock().unwrap();
                    state.prompts_in_flight = state.prompts_in_flight.saturating_add(1);
                }
                let sent = inner
                    .tx
                    .send(ClientCommand::Prompt {
                        session_id: session_id.to_owned(),
                        text,
                        admitted: admitted_tx,
                        reply: reply_tx,
                    })
                    .await;
                match sent {
                    Err(error) => {
                        inner.state.lock().unwrap().prompts_in_flight -= 1;
                        Err(anyhow::Error::new(error).context("Devin ACP actor stopped"))
                    }
                    Ok(()) => match tokio::time::timeout(RPC_TIMEOUT, admitted_rx).await {
                        Ok(Ok(Ok(()))) => Ok(reply_rx),
                        Ok(Ok(Err(error))) => {
                            inner.state.lock().unwrap().prompts_in_flight -= 1;
                            Err(anyhow::anyhow!("Devin ACP prompt write failed: {error}"))
                        }
                        Ok(Err(_)) => {
                            inner.state.lock().unwrap().prompts_in_flight -= 1;
                            Err(anyhow::anyhow!("Devin ACP actor dropped prompt admission"))
                        }
                        Err(_) => {
                            inner.state.lock().unwrap().prompts_in_flight -= 1;
                            Err(anyhow::anyhow!("Devin ACP prompt admission timed out"))
                        }
                    },
                }
            }
            #[cfg(test)]
            Self::Fake(fake) => fake.lock().unwrap().prompt(session_id, text),
        }
    }

    async fn session_cancel(&self, session_id: &str) -> Result<()> {
        self.notify("session/cancel", json!({"sessionId": session_id}))
            .await
    }

    async fn session_load(&self, session_id: &str, cwd: &Path) -> Result<()> {
        let params = json!({
            "sessionId": session_id,
            "cwd": cwd,
            "mcpServers": [],
        });
        self.request("session/load", params).await?;
        match self {
            Self::Stdio(inner) => inner.state.lock().unwrap().session_loaded = true,
            #[cfg(test)]
            Self::Fake(_) => {}
        }
        Ok(())
    }

    async fn shutdown(&self) {
        match self {
            Self::Stdio(inner) => {
                let _ = inner.tx.send(ClientCommand::Shutdown).await;
                let actor = inner.actor.lock().unwrap().take();
                if let Some(actor) = actor {
                    let _ = actor.await;
                }
            }
            #[cfg(test)]
            Self::Fake(fake) => fake.lock().unwrap().dead = true,
        }
    }

    fn snapshot(&self) -> AcpShared {
        match self {
            Self::Stdio(inner) => {
                let state = inner.state.lock().unwrap();
                AcpShared {
                    session_id: state.session_id.clone(),
                    session_loaded: state.session_loaded,
                    prompts_in_flight: state.prompts_in_flight,
                    last_stop_reason: state.last_stop_reason.clone(),
                    last_prompt_error: state.last_prompt_error.clone(),
                    terminal_error: state.terminal_error.clone(),
                    assistant_text: state.assistant_text.clone(),
                    last_message_id: state.last_message_id.clone(),
                    usage: state.usage.clone(),
                    pending_permissions: state.pending_permissions,
                    observed_model: state.observed_model.clone(),
                }
            }
            #[cfg(test)]
            Self::Fake(fake) => {
                let fake = fake.lock().unwrap();
                let state = fake.state.lock().unwrap();
                AcpShared {
                    session_id: state.session_id.clone(),
                    session_loaded: state.session_loaded,
                    prompts_in_flight: state.prompts_in_flight,
                    last_stop_reason: state.last_stop_reason.clone(),
                    last_prompt_error: state.last_prompt_error.clone(),
                    terminal_error: state.terminal_error.clone(),
                    assistant_text: state.assistant_text.clone(),
                    last_message_id: state.last_message_id.clone(),
                    usage: state.usage.clone(),
                    pending_permissions: state.pending_permissions,
                    observed_model: state.observed_model.clone(),
                }
            }
        }
    }

    fn capabilities(&self) -> AcpCapabilities {
        match self {
            Self::Stdio(inner) => inner.capabilities.lock().unwrap().clone(),
            #[cfg(test)]
            Self::Fake(fake) => fake.lock().unwrap().capabilities.clone(),
        }
    }

    fn shared_state(&self) -> Arc<Mutex<AcpShared>> {
        match self {
            Self::Stdio(inner) => Arc::clone(&inner.state),
            #[cfg(test)]
            Self::Fake(fake) => Arc::clone(&fake.lock().unwrap().state),
        }
    }

    fn diagnostics_tail(&self) -> String {
        match self {
            Self::Stdio(inner) => truncate_tail(&inner.tail.lock().unwrap()),
            #[cfg(test)]
            Self::Fake(_) => String::new(),
        }
    }
}

/// Mark a prompt's `session/prompt` response as consumed into the shared turn
/// state once the turn completes.
fn spawn_prompt_waiter(
    state: Arc<Mutex<AcpShared>>,
    rx: oneshot::Receiver<std::result::Result<Value, String>>,
) {
    tokio::spawn(async move {
        let outcome = rx.await;
        let mut state = state.lock().unwrap();
        state.prompts_in_flight = state.prompts_in_flight.saturating_sub(1);
        match outcome {
            Ok(Ok(result)) => {
                state.last_stop_reason = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Some("unknown".to_owned()));
            }
            Ok(Err(error)) => {
                state.last_prompt_error = Some(bound_text(&error, MAX_ERROR_BYTES));
            }
            Err(_) => {
                state.last_prompt_error = Some(
                    "Devin ACP prompt response channel closed before the turn ended".to_owned(),
                );
            }
        }
    });
}

#[derive(Clone, Copy, Default)]
struct AcpSpawnSpec<'a> {
    model: Option<&'a str>,
    agent: Option<&'a str>,
    cloud: bool,
}

async fn spawn_acp(
    session: &config::Session,
    task_id: Option<Uuid>,
    store: &TaskStore,
    lease: Option<Arc<TaskRuntimeLease>>,
    binary: &Path,
    spec: AcpSpawnSpec<'_>,
) -> Result<AcpClient> {
    #[cfg(test)]
    if let Some(hook) = spawn_hook() {
        return hook(session, task_id, store);
    }
    let owner = SessionInstance::from_session(session);
    let _permit = ensure_current_active_instance(&owner, session).await?;
    let mut last_error = None;
    for attempt in 0..ACP_SPAWN_ATTEMPTS {
        match spawn_acp_once(session, task_id, store, lease.clone(), binary, spec).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                let shutting_down = session_instance_is_closing(&owner);
                if shutting_down {
                    return Err(error);
                }
                last_error = Some(error);
                if attempt + 1 < ACP_SPAWN_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Devin ACP spawn failed")))
}

async fn spawn_acp_once(
    session: &config::Session,
    task_id: Option<Uuid>,
    store: &TaskStore,
    lease: Option<Arc<TaskRuntimeLease>>,
    binary: &Path,
    spec: AcpSpawnSpec<'_>,
) -> Result<AcpClient> {
    let owner = SessionInstance::from_session(session);
    let mut command = tokio::process::Command::new(binary);
    command.arg("acp");
    if spec.cloud {
        command.arg("--cloud");
    } else {
        if let Some(model) = spec.model {
            command.arg("--model").arg(model);
        }
        if let Some(agent) = spec.agent {
            command.arg("--agent-type").arg(agent);
        }
    }
    command
        .current_dir(record_scope_or_session(session, store, task_id))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .env_clear();
    for (key, value) in filtered_acp_environment(std::env::vars_os()) {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start devin acp from {}", binary.display()))?;
    let stdin = child.stdin.take().context("Devin ACP stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Devin ACP stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("Devin ACP stderr unavailable")?;

    let tail = Arc::new(Mutex::new(String::new()));
    drain_tail(stderr, Arc::clone(&tail));

    let state = Arc::new(Mutex::new(AcpShared::default()));
    let capabilities = Arc::new(Mutex::new(AcpCapabilities::default()));
    let binding = task_id.map(|task_id| TaskBinding {
        session: session.clone(),
        task_id,
        owner: owner.clone(),
        store: store.clone(),
    });
    let (tx, rx) = mpsc::channel(64);
    let actor = tokio::spawn(run_actor(
        child,
        stdin,
        stdout,
        binding,
        Arc::clone(&state),
        rx,
        lease,
    ));
    Ok(AcpClient::Stdio(Arc::new(StdioAcp {
        tx,
        actor: Arc::new(Mutex::new(Some(actor))),
        state,
        capabilities,
        tail,
    })))
}

fn record_scope_or_session(
    session: &config::Session,
    store: &TaskStore,
    task_id: Option<Uuid>,
) -> PathBuf {
    if let Some(task_id) = task_id
        && let Ok(record) = store.read_record(task_id)
    {
        return record.scope_cwd;
    }
    session.cwd.clone()
}

fn filtered_acp_environment<I>(environment: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    environment
        .into_iter()
        .filter(|(key, _)| acp_environment_key_allowed(key))
        .collect()
}

fn acp_environment_key_allowed(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    key.starts_with("LC_") || ACP_CHILD_ENV_ALLOWLIST.contains(&key)
}

async fn run_actor(
    mut child: tokio::process::Child,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    binding: Option<TaskBinding>,
    state: Arc<Mutex<AcpShared>>,
    mut commands: mpsc::Receiver<ClientCommand>,
    runtime_lease_guard: Option<Arc<TaskRuntimeLease>>,
) {
    let mut reader = BufReader::new(stdout);
    let (server_tx, mut server_rx) = mpsc::channel::<ServerResponse>(16);
    let mut next_id = 1u64;
    let mut pending = HashMap::<u64, oneshot::Sender<std::result::Result<Value, String>>>::new();
    let terminal_error = loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    break "client channel closed".to_owned();
                };
                match command {
                    ClientCommand::Request { method, params, reply } => {
                        let id = next_id;
                        next_id = next_id.saturating_add(1);
                        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                        if let Err(error) = write_json_line(&mut stdin, &message).await {
                            let _ = reply.send(Err(format!("Devin ACP write failed: {error:#}")));
                            break format!("Devin ACP write failed: {error:#}");
                        }
                        pending.insert(id, reply);
                    }
                    ClientCommand::Prompt { session_id, text, admitted, reply } => {
                        let id = next_id;
                        next_id = next_id.saturating_add(1);
                        let message = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": "session/prompt",
                            "params": {
                                "sessionId": session_id,
                                "prompt": [{"type": "text", "text": text}],
                            },
                        });
                        match write_json_line(&mut stdin, &message).await {
                            Ok(()) => {
                                let _ = admitted.send(Ok(()));
                                pending.insert(id, reply);
                            }
                            Err(error) => {
                                let _ = admitted.send(Err(format!("Devin ACP write failed: {error:#}")));
                                let _ = reply.send(Err(format!("Devin ACP write failed: {error:#}")));
                                break format!("Devin ACP write failed: {error:#}");
                            }
                        }
                    }
                    ClientCommand::Notify { method, params } => {
                        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
                        if let Err(error) = write_json_line(&mut stdin, &message).await {
                            break format!("Devin ACP notification write failed: {error:#}");
                        }
                    }
                    ClientCommand::Shutdown => break "shutdown requested".to_owned(),
                }
            }
            response = server_rx.recv() => {
                if let Some(response) = response {
                    let message = match response.payload {
                        Ok(result) => json!({"jsonrpc": "2.0", "id": response.id, "result": result}),
                        Err((code, message)) => json!({"jsonrpc": "2.0", "id": response.id, "error": {"code": code, "message": message}}),
                    };
                    if let Err(error) = write_json_line(&mut stdin, &message).await {
                        break format!("Devin ACP server-response write failed: {error:#}");
                    }
                }
            }
            line = read_bounded_json_line(&mut reader) => {
                match line {
                    Ok(Some(value)) => {
                        if let Some(method) = value.get("method").and_then(Value::as_str) {
                            let method = method.to_owned();
                            if let Some(id) = value.get("id").cloned() {
                                let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
                                let tx = server_tx.clone();
                                let binding = binding.as_ref().map(|binding| TaskBinding {
                                    session: binding.session.clone(),
                                    task_id: binding.task_id,
                                    owner: binding.owner.clone(),
                                    store: binding.store.clone(),
                                });
                                let state = Arc::clone(&state);
                                tokio::spawn(async move {
                                    let payload = handle_server_request(
                                        binding.as_ref(),
                                        &state,
                                        &method,
                                        params,
                                    ).await;
                                    let _ = tx.send(ServerResponse { id, payload }).await;
                                });
                            } else {
                                handle_notification(&state, &method, value.get("params"));
                            }
                        } else if let Some(id) = value.get("id").and_then(Value::as_u64)
                            && let Some(reply) = pending.remove(&id)
                        {
                            if let Some(error) = value.get("error") {
                                let _ = reply.send(Err(format!("Devin ACP RPC error: {error}")));
                            } else if let Some(result) = value.get("result") {
                                let _ = reply.send(Ok(result.clone()));
                            } else {
                                let _ = reply.send(Err("Devin ACP response has neither result nor error".to_owned()));
                            }
                        } else if let Some(error) = value.get("error") {
                            // Error response we cannot attribute (e.g. an
                            // id:null parse error). Fail the only pending
                            // request when unambiguous; otherwise surface it
                            // instead of silently dropping it into a timeout.
                            if pending.len() == 1 {
                                if let Some((_, reply)) = pending.drain().next() {
                                    let _ = reply.send(Err(format!("Devin ACP RPC error: {error}")));
                                }
                            } else {
                                state.lock().unwrap().last_prompt_error = Some(bound_text(
                                    &format!("Devin ACP server error: {error}"),
                                    MAX_ERROR_BYTES,
                                ));
                            }
                        }
                    }
                    Ok(None) => break "Devin ACP stdout closed".to_owned(),
                    Err(error) => break format!("Devin ACP protocol error: {error:#}"),
                }
            }
            status = child.wait() => {
                break match status {
                    Ok(status) => format!("Devin ACP exited: {status}"),
                    Err(error) => format!("Devin ACP wait failed: {error}"),
                };
            }
        }
    };

    for (_, reply) in pending {
        let _ = reply.send(Err(terminal_error.clone()));
    }
    {
        let mut state = state.lock().unwrap();
        state.terminal_error = Some(bound_text(&terminal_error, MAX_ERROR_BYTES));
    }
    let _ = child.kill().await;
    drop(runtime_lease_guard);
}

async fn write_json_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() <= MAX_RPC_LINE_BYTES,
        "outbound Devin ACP message exceeds {MAX_RPC_LINE_BYTES} bytes"
    );
    stdin.write_all(&bytes).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_bounded_json_line<R>(reader: &mut R) -> Result<Option<Value>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
            anyhow::ensure!(
                line.len().saturating_add(newline) <= MAX_RPC_LINE_BYTES,
                "Devin ACP message exceeds {MAX_RPC_LINE_BYTES} bytes"
            );
            line.extend_from_slice(&chunk[..newline]);
            reader.consume(newline + 1);
            break;
        }
        anyhow::ensure!(
            line.len().saturating_add(chunk.len()) <= MAX_RPC_LINE_BYTES,
            "Devin ACP message exceeds {MAX_RPC_LINE_BYTES} bytes"
        );
        line.extend_from_slice(chunk);
        let len = chunk.len();
        reader.consume(len);
    }
    let value = serde_json::from_slice(&line).context("invalid Devin ACP JSON line")?;
    Ok(Some(value))
}

fn handle_notification(state: &Arc<Mutex<AcpShared>>, method: &str, params: Option<&Value>) {
    if method != "session/update" {
        return;
    }
    let Some(params) = params else {
        return;
    };
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    let Some(update) = params.get("update") else {
        return;
    };
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    let mut state = state.lock().unwrap();
    if state.session_id.as_deref() != Some(session_id) {
        return;
    }
    match kind {
        Some("agent_message_chunk") => {
            let message_id = update
                .get("messageId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(message_id) = message_id
                && state.last_message_id.as_deref() != Some(message_id.as_str())
            {
                state.assistant_text.clear();
                state.last_message_id = Some(message_id);
            }
            if let Some(text) = update
                .get("content")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
                && state.assistant_text.len() < MAX_ASSISTANT_TEXT_BYTES
            {
                let remaining = MAX_ASSISTANT_TEXT_BYTES - state.assistant_text.len();
                let take = text.len().min(remaining);
                state.assistant_text.push_str(&text[..take]);
            }
        }
        Some("usage_update") => {
            let mut usage = BTreeMap::new();
            for (key, field) in [("context_used", "used"), ("context_size", "size")] {
                if let Some(value) = update.get(field).and_then(Value::as_u64) {
                    usage.insert(key.to_owned(), value);
                }
            }
            if !usage.is_empty() {
                state.usage = Some(usage);
            }
        }
        _ => {}
    }
}

async fn handle_server_request(
    binding: Option<&TaskBinding>,
    state: &Arc<Mutex<AcpShared>>,
    method: &str,
    params: Value,
) -> std::result::Result<Value, (i64, String)> {
    match method {
        "session/request_permission" => {
            let Some(binding) = binding else {
                return Err((
                    -32601,
                    "permission request is unavailable outside a Devin task".to_owned(),
                ));
            };
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| (-32602, "permission request is missing sessionId".to_owned()))?;
            {
                let mut shared = state.lock().unwrap();
                if shared.session_id.as_deref() != Some(session_id) {
                    return Err((
                        -32602,
                        "permission request does not match the task session".to_owned(),
                    ));
                }
                shared.pending_permissions = shared.pending_permissions.saturating_add(1);
            }
            mark_task_waiting_approval(binding, true);
            let (detail, metadata) = permission_approval(binding.task_id, &params);
            let allowed = request_child_approval(
                &binding.session,
                &binding.owner,
                "Devin tool permission",
                detail,
                metadata,
            )
            .await;
            mark_task_waiting_approval(binding, false);
            let mut state = state.lock().unwrap();
            state.pending_permissions = state.pending_permissions.saturating_sub(1);
            if !allowed {
                return Ok(json!({"outcome": {"outcome": "cancelled"}}));
            }
            let option_id = select_permission_option(&params);
            match option_id {
                Some(option_id) => Ok(json!({
                    "outcome": {"outcome": "selected", "optionId": option_id}
                })),
                None => Ok(json!({"outcome": {"outcome": "cancelled"}})),
            }
        }
        _ => Err((
            -32601,
            format!("unsupported Devin ACP request method: {method}"),
        )),
    }
}

fn select_permission_option(params: &Value) -> Option<String> {
    let options = params.get("options").and_then(Value::as_array)?;
    let by_kind = |kind: &str| {
        options.iter().find_map(|option| {
            (option.get("kind").and_then(Value::as_str) == Some(kind))
                .then(|| option.get("optionId").and_then(Value::as_str))
                .flatten()
                .map(str::to_owned)
        })
    };
    by_kind("allow_once")
        .or_else(|| by_kind("allow_always"))
        .or_else(|| {
            options
                .first()
                .and_then(|option| option.get("optionId"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn mark_task_waiting_approval(binding: &TaskBinding, waiting: bool) {
    let _ = binding
        .store
        .update(&binding.session, binding.task_id, |record| {
            if record.status.is_terminal() {
                return Ok(());
            }
            record.status = if waiting {
                TaskStatus::WaitingApproval
            } else if matches!(record.status, TaskStatus::WaitingApproval) {
                TaskStatus::Running
            } else {
                record.status
            };
            record.revision = record.revision.saturating_add(1);
            Ok(())
        });
}

fn permission_approval(task_id: Uuid, params: &Value) -> (String, BTreeMap<String, String>) {
    let tool_call = params.get("toolCall").cloned().unwrap_or_else(|| json!({}));
    let tool_call_id = tool_call
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(|value| bound_text(value, 128))
        .unwrap_or_else(|| "(unknown)".to_owned());
    let title = tool_call
        .get("title")
        .and_then(Value::as_str)
        .map(|value| bound_text(value, 256))
        .unwrap_or_else(|| "(no title)".to_owned());
    let kind = tool_call
        .get("kind")
        .and_then(Value::as_str)
        .map(|value| bound_text(value, 64))
        .unwrap_or_else(|| "(unknown)".to_owned());
    let metadata = BTreeMap::from([
        ("provenance".to_owned(), "devin_acp".to_owned()),
        ("source".to_owned(), "devin_delegation".to_owned()),
        ("tool".to_owned(), "session/request_permission".to_owned()),
        ("operation_type".to_owned(), "tool_permission".to_owned()),
        ("target".to_owned(), format!("task:{task_id}")),
        ("task_id".to_owned(), task_id.to_string()),
        ("mutation".to_owned(), "true".to_owned()),
        ("read_only".to_owned(), "false".to_owned()),
        ("scope".to_owned(), "session_cwd".to_owned()),
        ("tool_call_id".to_owned(), tool_call_id.clone()),
        ("tool_kind".to_owned(), kind.clone()),
    ]);
    (
        format!(
            "Devin tool permission request\ntool call: {title}\nkind: {kind}\ntarget: task {task_id}"
        ),
        metadata,
    )
}

async fn request_child_approval(
    session: &config::Session,
    owner: &SessionInstance,
    operation: &str,
    detail: String,
    metadata: BTreeMap<String, String>,
) -> bool {
    let permit = match ensure_current_active_instance(owner, session).await {
        Ok(permit) => permit,
        Err(_) => return false,
    };
    let mut cancellation = permit.cancellation.clone();
    tokio::select! {
        result = async {
            let allowed = approvals::request_user_approval_for_instance(
                session,
                operation,
                detail,
                session.cwd.clone(),
                metadata,
            )
            .await
            .unwrap_or(false);
            if allowed {
                ensure_current_active_instance(owner, session).await.is_ok()
            } else {
                false
            }
        } => result,
        changed = cancellation.changed() => {
            let _ = changed;
            false
        }
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
                    if tail.len() > ACP_TAIL_BYTES {
                        let cut = tail.len() - ACP_TAIL_BYTES;
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
    client: AcpClient,
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
    client: AcpClient,
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
    client: AcpClient,
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
            "Devin session instance is closing"
        );
        let mut state = runtimes().lock().unwrap();
        anyhow::ensure!(
            !state.contains_key(&task_id),
            "Devin task runtime is already registered"
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
                    "failed to finalize Devin tasks after session {} stopped: {error:#}",
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
            .context("failed to finalize Devin tasks for ended session instance")?;
        if !outcome.deferred {
            let _ = outcome.finalized;
            remove_session_evidence(owner).await;
            finish_session_shutdown(owner)?;
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out waiting for a remotely owned Devin task runtime to stop"
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
            Err(error).context("cannot open Devin task store for session cleanup")
        }
    }
}

// ---------- reconciliation ----------

struct DerivedAcpState {
    status: TaskStatus,
    usage: Option<BTreeMap<String, u64>>,
    observed_model: Option<String>,
    report: Option<Value>,
    last_error: Option<String>,
}

fn derive_acp_state(record: &TaskRecord, snap: &AcpShared) -> DerivedAcpState {
    if snap.pending_permissions > 0 {
        return DerivedAcpState {
            status: TaskStatus::WaitingApproval,
            usage: snap.usage.clone(),
            observed_model: snap.observed_model.clone(),
            report: None,
            last_error: Some("devin acp permission request is pending".to_owned()),
        };
    }
    if snap.prompts_in_flight > 0 {
        return DerivedAcpState {
            status: TaskStatus::Running,
            usage: snap.usage.clone(),
            observed_model: snap.observed_model.clone(),
            report: None,
            last_error: None,
        };
    }
    if let Some(reason) = &snap.last_stop_reason {
        return match reason.as_str() {
            "end_turn" => {
                let (report, report_error) = extract_report(&snap.assistant_text);
                DerivedAcpState {
                    status: TaskStatus::Completed,
                    usage: snap.usage.clone(),
                    observed_model: snap.observed_model.clone(),
                    report,
                    last_error: report_error,
                }
            }
            "cancelled" => DerivedAcpState {
                status: TaskStatus::Interrupted,
                usage: snap.usage.clone(),
                observed_model: snap.observed_model.clone(),
                report: None,
                last_error: None,
            },
            other => DerivedAcpState {
                status: TaskStatus::RetryableFailed,
                usage: snap.usage.clone(),
                observed_model: snap.observed_model.clone(),
                report: None,
                last_error: Some(bound_text(
                    &format!("devin acp turn ended with stop reason {other}"),
                    MAX_ERROR_BYTES,
                )),
            },
        };
    }
    if let Some(error) = &snap.last_prompt_error {
        return DerivedAcpState {
            status: TaskStatus::RetryableFailed,
            usage: snap.usage.clone(),
            observed_model: snap.observed_model.clone(),
            report: None,
            last_error: Some(error.clone()),
        };
    }
    if let Some(error) = &snap.terminal_error {
        return DerivedAcpState {
            status: TaskStatus::ReconciliationRequired,
            usage: snap.usage.clone(),
            observed_model: snap.observed_model.clone(),
            report: None,
            last_error: Some(error.clone()),
        };
    }
    if snap.session_loaded
        && matches!(
            record.status,
            TaskStatus::Running
                | TaskStatus::Accepted
                | TaskStatus::Unknown
                | TaskStatus::ReconciliationRequired
        )
    {
        // The acp child was respawned and the session reloaded, but the turn
        // that was in flight is gone. Its outcome is unknowable from here.
        return DerivedAcpState {
            status: TaskStatus::ReconciliationRequired,
            usage: snap.usage.clone(),
            observed_model: snap.observed_model.clone(),
            report: None,
            last_error: Some("devin acp turn state was lost across a reload".to_owned()),
        };
    }
    DerivedAcpState {
        status: record.status,
        usage: snap.usage.clone(),
        observed_model: snap.observed_model.clone(),
        report: None,
        last_error: None,
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
    client: &AcpClient,
) -> Result<TaskRecord> {
    let Some(acp_session_id) = record.acp_session_id.clone() else {
        // Accepted but never bound to an acp session: the spawn failed before
        // session/new, or the crash landed in that window. The task text is
        // never persisted, so this cannot be re-driven safely.
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

    if !client.snapshot().session_loaded {
        let scope = record.scope_cwd.clone();
        anyhow::ensure!(
            client.capabilities().load_session,
            "devin acp does not advertise loadSession; the retained session cannot be reattached"
        );
        match client.session_load(&acp_session_id, &scope).await {
            Ok(()) => {}
            Err(_) if session_instance_is_closing(owner) => {
                return store.update_if_instance_live(session, record.task_id, owner, |record| {
                    if record.status.is_terminal() {
                        return Ok(());
                    }
                    record.status = TaskStatus::Interrupted;
                    record.revision = record.revision.saturating_add(1);
                    Ok(())
                });
            }
            Err(error) => {
                let error = bound_text(&format!("{error:#}"), MAX_ERROR_BYTES);
                return store.update_if_instance_live(session, record.task_id, owner, |record| {
                    if record.status.is_terminal() {
                        return Ok(());
                    }
                    record.status = TaskStatus::ReconciliationRequired;
                    record.last_error = Some(error.clone());
                    record.revision = record.revision.saturating_add(1);
                    Ok(())
                });
            }
        }
    }

    let snap = client.snapshot();
    let derived = derive_acp_state(&record, &snap);
    apply_derived(session, owner, store, record, derived).await
}

async fn apply_derived(
    session: &config::Session,
    owner: &SessionInstance,
    store: &TaskStore,
    record: TaskRecord,
    derived: DerivedAcpState,
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

enum EnsuredRuntime {
    Local(AcpClient),
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
    let client = spawn_acp(
        session,
        Some(record.task_id),
        store,
        Some(Arc::clone(&lease)),
        binary,
        AcpSpawnSpec {
            model: record.model.as_deref(),
            agent: record.agent.as_deref(),
            cloud: record.cloud,
        },
    )
    .await?;
    acp_initialize(&client).await?;
    insert_runtime(session, record.task_id, store, client.clone(), lease).await?;
    Ok(EnsuredRuntime::Local(client))
}

async fn acp_initialize(client: &AcpClient) -> Result<Value> {
    let initialized = client
        .request(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                    "auth": {"terminal": false},
                },
                "clientInfo": {"name": ACP_CLIENT_NAME, "version": ACP_CLIENT_VERSION},
            }),
        )
        .await?;
    let capabilities = parse_initialize_response(&initialized)?;
    match client {
        AcpClient::Stdio(inner) => {
            *inner.capabilities.lock().unwrap() = capabilities;
        }
        #[cfg(test)]
        AcpClient::Fake(fake) => {
            fake.lock().unwrap().capabilities = capabilities;
        }
    }
    Ok(initialized)
}

fn parse_initialize_response(value: &Value) -> Result<AcpCapabilities> {
    let object = value
        .as_object()
        .context("Devin ACP initialize result must be an object")?;
    let protocol_version = object
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .context("Devin ACP initialize result is missing protocolVersion")?;
    anyhow::ensure!(
        protocol_version >= 1,
        "Devin ACP initialize result has invalid protocolVersion"
    );
    let capabilities = object
        .get("agentCapabilities")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let load_session = capabilities
        .get("loadSession")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let auth_methods = value
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|methods| methods.len())
        .unwrap_or(0);
    Ok(AcpCapabilities {
        load_session,
        session_capabilities: capabilities.get("sessionCapabilities").cloned(),
        prompt_capabilities: capabilities.get("promptCapabilities").cloned(),
        agent_info: value.get("agentInfo").cloned(),
        protocol_version: Some(protocol_version),
        auth_methods,
    })
}

// ---------- devin binary resolution ----------

pub(crate) fn resolve_devin_executable() -> Result<PathBuf, String> {
    match std::env::var("TEMOTE_DEVIN_BIN") {
        Ok(value) => resolve_devin_override(&value),
        Err(std::env::VarError::NotPresent) => Ok(PathBuf::from("devin")),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("TEMOTE_DEVIN_BIN is not a valid path value".to_owned())
        }
    }
}

fn resolve_devin_override(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err("TEMOTE_DEVIN_BIN is set but empty; unset it to use PATH lookup".to_owned());
    }
    if value.contains('\0') {
        return Err("TEMOTE_DEVIN_BIN is not a valid path value".to_owned());
    }
    if value.len() > MAX_ACP_BINARY_PATH_BYTES {
        return Err(format!(
            "TEMOTE_DEVIN_BIN exceeds the {MAX_ACP_BINARY_PATH_BYTES}-byte path limit"
        ));
    }
    if !Path::new(value).is_absolute() {
        return Err("TEMOTE_DEVIN_BIN must be an absolute path".to_owned());
    }
    let canonical = std::fs::canonicalize(value)
        .map_err(|_| "TEMOTE_DEVIN_BIN does not point to an existing file".to_owned())?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|_| "TEMOTE_DEVIN_BIN does not point to an existing file".to_owned())?;
    if !metadata.is_file() {
        return Err("TEMOTE_DEVIN_BIN must point to a regular file".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("TEMOTE_DEVIN_BIN does not point to an executable file".to_owned());
        }
    }
    Ok(canonical)
}

// ---------- public entry points ----------

pub(crate) async fn status(session: &config::Session) -> Result<Value> {
    let owner = SessionInstance::from_session(session);
    let _permit = ensure_current_active_instance(&owner, session).await?;
    let binary = resolve_devin_executable().map_err(anyhow::Error::msg)?;
    let store = TaskStore::default_store()?;
    let probe_id = Uuid::new_v4();
    let lease = Arc::new(
        store
            .try_acquire_runtime_lease(probe_id)?
            .context("cannot acquire Devin probe lease")?,
    );
    let client = spawn_acp(
        session,
        None,
        &store,
        Some(lease),
        &binary,
        AcpSpawnSpec::default(),
    )
    .await;
    let result = match client {
        Ok(client) => {
            let initialized = acp_initialize(&client).await;
            let tail = client.diagnostics_tail();
            client.shutdown().await;
            initialized.map(|initialized| (initialized, tail))
        }
        Err(error) => Err(error),
    };
    cleanup_probe_state(&store, probe_id);
    let (initialized, tail) = result?;
    let capabilities = parse_initialize_response(&initialized)?;
    Ok(json!({
        "compatible": true,
        "protocol_version": capabilities.protocol_version,
        "agent": capabilities.agent_info,
        "load_session": capabilities.load_session,
        "session_capabilities": capabilities.session_capabilities,
        "prompt_capabilities": capabilities.prompt_capabilities,
        "auth_methods": capabilities.auth_methods,
        "binary": binary,
        "diagnostics_tail": tail,
    }))
}

fn cleanup_probe_state(store: &TaskStore, probe_id: Uuid) {
    let _ = std::fs::remove_file(store.runtime_lock_path(probe_id));
    let _ = std::fs::remove_dir_all(store.runtime_state_directory(probe_id));
}

pub(crate) async fn task_start(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    let binary = resolve_devin_executable().map_err(anyhow::Error::msg)?;
    task_start_with_store_and_binary(args, session, &store, &binary).await
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
    let cloud = args
        .get("cloud")
        .map(|value| value.as_bool().context("cloud must be a boolean"))
        .transpose()?
        .unwrap_or(false);
    validate_task_input(task, "task")?;
    if let Some(model) = model {
        validate_argument(model, "model")?;
    }
    if let Some(agent) = agent {
        validate_argument(agent, "agent")?;
    }
    anyhow::ensure!(
        !cloud || (model.is_none() && agent.is_none()),
        "model and agent are ignored by `devin acp --cloud`; omit them when cloud is true"
    );

    let task_id = task_id_for_operation(session, operation_id)?;
    let request_fingerprint = fingerprint(&json!({
        "kind": "start",
        "task_id": task_id,
        "task": task,
        "model": model,
        "agent": agent,
        "cloud": cloud,
    }))?;
    let now = config::unix_time();
    let mut record = TaskRecord {
        schema_version: TASK_SCHEMA_VERSION,
        task_id,
        owner: SessionInstance::from_session(session),
        scope_cwd: config::canonical_directory(&session.cwd)?,
        model: model.map(str::to_owned),
        agent: agent.map(str::to_owned),
        cloud,
        status: TaskStatus::Accepted,
        revision: 1,
        generation: 0,
        acp_session_id: None,
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

    let client = match spawn_acp(
        session,
        Some(task_id),
        store,
        Some(Arc::clone(&runtime_lease)),
        binary,
        AcpSpawnSpec {
            model,
            agent,
            cloud,
        },
    )
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
        acp_initialize(&client).await?;
        let created = client
            .request(
                "session/new",
                json!({
                    "cwd": record.scope_cwd,
                    "mcpServers": [],
                }),
            )
            .await?;
        let acp_session_id = created
            .get("sessionId")
            .and_then(Value::as_str)
            .context("devin acp session/new response is missing sessionId")?
            .to_owned();
        let observed_model = created
            .get("models")
            .and_then(|models| models.get("currentModelId"))
            .and_then(Value::as_str)
            .map(|value| bound_text(value, MAX_ARGUMENT_BYTES));
        match &client {
            AcpClient::Stdio(inner) => {
                let mut state = inner.state.lock().unwrap();
                state.session_id = Some(acp_session_id.clone());
                state.session_loaded = true;
                state.observed_model = observed_model.clone();
            }
            #[cfg(test)]
            AcpClient::Fake(fake) => {
                let fake = fake.lock().unwrap();
                let mut state = fake.state.lock().unwrap();
                state.session_id = Some(acp_session_id.clone());
                state.session_loaded = true;
                state.observed_model = observed_model.clone();
            }
        }

        let bind_permit = ensure_current_active_instance(&owner, session).await?;
        record = store.update_if_instance_live(session, task_id, &owner, |record| {
            if record.status.is_terminal() {
                return Ok(());
            }
            anyhow::ensure!(
                record.acp_session_id.is_none()
                    || record.acp_session_id.as_deref() == Some(acp_session_id.as_str()),
                "devin acp task session was already bound"
            );
            record.acp_session_id = Some(acp_session_id.clone());
            if observed_model.is_some() {
                record.observed_model = observed_model.clone();
            }
            record.revision = record.revision.saturating_add(1);
            Ok(())
        })?;
        drop(bind_permit);

        let prompt_text = format!(
            "{}{}{}",
            REPORT_INSTRUCTIONS.replace("__REQUESTED_MODEL__", &serde_json::to_string(&model)?),
            "\n",
            task
        );
        let rx = client.session_prompt(&acp_session_id, prompt_text).await?;
        spawn_prompt_waiter(client.shared_state(), rx);

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
    let binary = resolve_devin_executable().map_err(anyhow::Error::msg)?;
    task_get_with_store_and_binary(args, session, &store, &binary).await
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
            .acp_session_id
            .clone()
            .map(|acp_session_id| {
                json!({
                    "kind": "devin_acp_task_final_state",
                    "task_id": task_id,
                    "acp_session_id": acp_session_id,
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
    let binary = resolve_devin_executable().map_err(anyhow::Error::msg)?;
    task_control_with_store_and_binary(args, session, &store, &binary).await
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
        "unsupported Devin task action"
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
    if record.acp_session_id.is_none() {
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
    let Some(acp_session_id) = record.acp_session_id.clone() else {
        let record = store.update(session, task_id, |record| {
            if !record.status.is_terminal() {
                record.status = TaskStatus::ReconciliationRequired;
                record.revision = record.revision.saturating_add(1);
            }
            Ok(())
        })?;
        return Ok(task_view(&record, None));
    };

    let result = match action {
        "steer" => {
            let rx = client
                .session_prompt(&acp_session_id, input.unwrap_or_default().to_owned())
                .await;
            match rx {
                Ok(rx) => {
                    spawn_prompt_waiter(client.shared_state(), rx);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
        "resume" => {
            if !client.snapshot().session_loaded {
                if !client.capabilities().load_session {
                    let shutting_down = session_instance_is_closing(&owner);
                    let record = apply_control_failure(
                        store,
                        session,
                        task_id,
                        operation_id,
                        shutting_down,
                    )?;
                    return Ok(task_view(&record, None));
                }
                match client
                    .session_load(&acp_session_id, &record.scope_cwd)
                    .await
                {
                    Ok(()) => {}
                    Err(error) => {
                        let _ = error;
                        let shutting_down = session_instance_is_closing(&owner);
                        let record = apply_control_failure(
                            store,
                            session,
                            task_id,
                            operation_id,
                            shutting_down,
                        )?;
                        return Ok(task_view(&record, None));
                    }
                }
            }
            match client
                .session_prompt(&acp_session_id, RESUME_INSTRUCTIONS.to_owned())
                .await
            {
                Ok(rx) => {
                    spawn_prompt_waiter(client.shared_state(), rx);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
        "interrupt" => client.session_cancel(&acp_session_id).await,
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
        // Interrupt tears the acp child down: release the runtime so the next
        // get/resume respawns a fresh child that reattaches via session/load.
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
type SpawnHook =
    dyn Fn(&config::Session, Option<Uuid>, &TaskStore) -> Result<AcpClient> + Send + Sync;

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
struct FakeAcp {
    dead: bool,
    next_session: u64,
    sessions: HashMap<String, bool>,
    state: Arc<Mutex<AcpShared>>,
    capabilities: AcpCapabilities,
    load_fail: Option<String>,
    new_fail: Option<String>,
    prompt_fail: Option<String>,
    prompt_calls: Vec<(String, String)>,
    cancel_calls: Vec<String>,
    prompt_senders: HashMap<String, oneshot::Sender<std::result::Result<Value, String>>>,
}

#[cfg(test)]
impl FakeAcp {
    fn session_id_of(&mut self) -> String {
        self.next_session += 1;
        format!("acp_{:04}", self.next_session)
    }

    fn require_live(&self) -> Result<()> {
        anyhow::ensure!(!self.dead, "fake devin acp stopped");
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.require_live()?;
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "agentCapabilities": {
                    "loadSession": self.capabilities.load_session,
                    "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
                    "sessionCapabilities": {},
                },
                "agentInfo": {"name": "devin", "version": "0.0.0-test"},
            })),
            "session/new" => {
                if let Some(error) = &self.new_fail {
                    anyhow::bail!("{error}");
                }
                let id = self.session_id_of();
                self.sessions.insert(id.clone(), false);
                Ok(json!({
                    "sessionId": id,
                    "models": {"currentModelId": "devin-test-model"},
                }))
            }
            "session/load" => {
                if let Some(error) = &self.load_fail {
                    anyhow::bail!("{error}");
                }
                if !self.capabilities.load_session {
                    anyhow::bail!("loadSession is not advertised");
                }
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .context("missing sessionId")?;
                anyhow::ensure!(
                    self.sessions.contains_key(session_id),
                    "unknown acp session {session_id}"
                );
                self.state.lock().unwrap().session_id = Some(session_id.to_owned());
                self.state.lock().unwrap().session_loaded = true;
                Ok(json!({}))
            }
            _ => anyhow::bail!("unsupported fake method {method}"),
        }
    }

    fn prompt(
        &mut self,
        session_id: &str,
        text: String,
    ) -> Result<oneshot::Receiver<std::result::Result<Value, String>>> {
        self.require_live()?;
        if let Some(error) = &self.prompt_fail {
            anyhow::bail!("{error}");
        }
        anyhow::ensure!(
            self.sessions.contains_key(session_id),
            "unknown acp session {session_id}"
        );
        {
            let mut state = self.state.lock().unwrap();
            state.prompts_in_flight = state.prompts_in_flight.saturating_add(1);
        }
        self.prompt_calls.push((session_id.to_owned(), text));
        let (reply_tx, reply_rx) = oneshot::channel();
        self.prompt_senders.insert(session_id.to_owned(), reply_tx);
        Ok(reply_rx)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.require_live()?;
        if method == "session/cancel"
            && let Some(session_id) = params.get("sessionId").and_then(Value::as_str)
        {
            self.cancel_calls.push(session_id.to_owned());
            if let Some(sender) = self.prompt_senders.remove(session_id) {
                let _ = sender.send(Ok(json!({"stopReason": "cancelled"})));
            }
        }
        Ok(())
    }

    fn complete_prompt(&mut self, session_id: &str, stop_reason: &str, text: Option<&str>) {
        if let Some(text) = text {
            let mut state = self.state.lock().unwrap();
            state.last_message_id = Some(format!("msg-{}", Uuid::new_v4().simple()));
            state.assistant_text.clear();
            state.assistant_text.push_str(text);
        }
        if let Some(sender) = self.prompt_senders.remove(session_id) {
            let _ = sender.send(Ok(json!({"stopReason": stop_reason})));
        }
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
        format!("devin-acp-test-{}", Uuid::new_v4().simple())
    }

    fn fake_client(load_session: bool) -> (AcpClient, Arc<Mutex<FakeAcp>>) {
        let fake = Arc::new(Mutex::new(FakeAcp {
            capabilities: AcpCapabilities {
                load_session,
                ..AcpCapabilities::default()
            },
            ..FakeAcp::default()
        }));
        (AcpClient::Fake(Arc::clone(&fake)), fake)
    }

    fn install_fake(fake: Arc<Mutex<FakeAcp>>) {
        let hook: Arc<SpawnHook> = Arc::new(move |_, _, _| Ok(AcpClient::Fake(Arc::clone(&fake))));
        *SPAWN_HOOK.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(hook);
    }

    fn clear_fake() {
        *SPAWN_HOOK.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devin-acp-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_store(root: &Path) -> TaskStore {
        TaskStore::new(root.join("devin-acp-tasks"))
    }

    fn start_args(operation_id: Uuid, task: &str) -> Value {
        json!({
            "operation_id": operation_id,
            "task": task,
            "model": "devin-test-model",
        })
    }

    async fn wait_for_prompt_idle(fake: &Arc<Mutex<FakeAcp>>) {
        for _ in 0..100 {
            let state = fake.lock().unwrap().state.clone();
            if state.lock().unwrap().prompts_in_flight == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("prompt did not settle");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_persists_acceptance_and_drives_acp() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (client, fake) = fake_client(true);
        install_fake(fake.clone());

        let op = Uuid::new_v4();
        let out = task_start_with_store_and_binary(
            &start_args(op, "implement the feature"),
            &session,
            &store,
            Path::new("devin"),
        )
        .await
        .unwrap();
        clear_fake();

        assert_eq!(out["status"], "running");
        let task_id = Uuid::parse_str(out["task_id"].as_str().unwrap()).unwrap();
        assert_eq!(task_id, task_id_for_operation(&session, op).unwrap());
        let fake = fake.lock().unwrap();
        assert_eq!(fake.prompt_calls.len(), 1);
        let (session_id, text) = &fake.prompt_calls[0];
        assert!(fake.sessions.contains_key(session_id));
        assert!(text.contains("implement the feature"));
        let _ = client;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_replays_same_operation_id_without_respawning() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client(true);
        install_fake(fake.clone());

        let op = Uuid::new_v4();
        let first = task_start_with_store_and_binary(
            &start_args(op, "same task"),
            &session,
            &store,
            Path::new("devin"),
        )
        .await
        .unwrap();
        let second = task_start_with_store_and_binary(
            &start_args(op, "same task"),
            &session,
            &store,
            Path::new("devin"),
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
        let (_, fake) = fake_client(true);
        install_fake(fake);

        let op = Uuid::new_v4();
        task_start_with_store_and_binary(
            &start_args(op, "original task"),
            &session,
            &store,
            Path::new("devin"),
        )
        .await
        .unwrap();
        let error = task_start_with_store_and_binary(
            &start_args(op, "different task"),
            &session,
            &store,
            Path::new("devin"),
        )
        .await
        .unwrap_err();
        clear_fake();
        assert!(format!("{error:#}").contains("OPERATION_CONFLICT"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_cloud_rejects_model_and_agent() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client(true);
        install_fake(fake.clone());

        let mut args = start_args(Uuid::new_v4(), "do the thing");
        args["cloud"] = json!(true);
        let error = task_start_with_store_and_binary(&args, &session, &store, Path::new("devin"))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("ignored by `devin acp --cloud`"));

        let mut args = json!({
            "operation_id": Uuid::new_v4(),
            "task": "do the thing",
            "cloud": true,
        });
        task_start_with_store_and_binary(&args, &session, &store, Path::new("devin"))
            .await
            .unwrap();
        args["cloud"] = json!("yes");
        let error = task_start_with_store_and_binary(&args, &session, &store, Path::new("devin"))
            .await
            .unwrap_err();
        clear_fake();
        assert!(format!("{error:#}").contains("cloud must be a boolean"));
        assert!(fake.lock().unwrap().prompt_calls.len() == 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_completes_from_stop_reason_and_report() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client(true);
        install_fake(fake.clone());

        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "do the thing"),
            &session,
            &store,
            Path::new("devin"),
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
            "requested_model": "devin-test-model",
            "requested_effort": null,
            "observed_model": "devin-test-model",
            "observed_effort": null,
        });
        fake.lock()
            .unwrap()
            .complete_prompt(&session_id, "end_turn", Some(&report.to_string()));
        wait_for_prompt_idle(&fake).await;

        let out = task_get_with_store_and_binary(
            &json!({"task_id": task_id}),
            &session,
            &store,
            Path::new("devin"),
        )
        .await
        .unwrap();
        clear_fake();

        assert_eq!(out["status"], "completed");
        assert_eq!(out["report"]["summary"], "done");
        assert_eq!(out["observed_model"], "devin-test-model");
        assert!(out["evidence"].is_object());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_maps_cancelled_to_interrupted() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client(true);
        install_fake(fake.clone());

        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "work"),
            &session,
            &store,
            Path::new("devin"),
        )
        .await
        .unwrap();
        let task_id = out["task_id"].as_str().unwrap().to_owned();
        let session_id = {
            let fake = fake.lock().unwrap();
            fake.sessions.keys().next().unwrap().clone()
        };

        let out = task_control_with_store_and_binary(
            &json!({
                "task_id": task_id,
                "operation_id": Uuid::new_v4(),
                "action": "interrupt",
            }),
            &session,
            &store,
            Path::new("devin"),
        )
        .await
        .unwrap();
        assert_eq!(out["status"], "interrupted");
        wait_for_prompt_idle(&fake).await;
        clear_fake();
        let fake = fake.lock().unwrap();
        assert_eq!(fake.cancel_calls, vec![session_id]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_deferred_when_lease_held_elsewhere() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client(true);
        install_fake(fake.clone());

        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "work"),
            &session,
            &store,
            Path::new("devin"),
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
            Path::new("devin"),
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
        let (_, fake) = fake_client(true);
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
            Path::new("devin"),
        )
        .await
        .unwrap();
        clear_fake();
        assert_eq!(out["status"], "reconciliation_required");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn control_steer_sends_prompt_and_bumps_generation() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client(true);
        install_fake(fake.clone());

        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "task"),
            &session,
            &store,
            Path::new("devin"),
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
            Path::new("devin"),
        )
        .await
        .unwrap();
        clear_fake();
        assert_eq!(out["status"], "running");
        let fake = fake.lock().unwrap();
        assert_eq!(fake.prompt_calls.len(), 2);
        assert_eq!(fake.prompt_calls[1].1, "also fix the typo");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn control_resume_rejects_running_task() {
        let root = tempdir();
        let (_handle, session) = active_test_session(&root, &test_id(), false).await;
        let _serial = serial().await;
        let store = test_store(&root);
        let (_, fake) = fake_client(true);
        install_fake(fake.clone());
        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "task"),
            &session,
            &store,
            Path::new("devin"),
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
            Path::new("devin"),
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
        let (_, fake) = fake_client(true);
        install_fake(fake);
        let out = task_start_with_store_and_binary(
            &start_args(Uuid::new_v4(), "task"),
            &session_a,
            &store,
            Path::new("devin"),
        )
        .await
        .unwrap();
        let error = task_get_with_store_and_binary(
            &json!({"task_id": out["task_id"]}),
            &session_b,
            &store,
            Path::new("devin"),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("DEVIN_TASK_NOT_FOUND"));
    }

    fn record_for(
        session: &config::Session,
        task_id: Uuid,
        status: TaskStatus,
        acp_session_id: Option<&str>,
    ) -> TaskRecord {
        let now = config::unix_time();
        TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(session),
            scope_cwd: config::canonical_directory(&session.cwd).unwrap(),
            model: None,
            agent: None,
            cloud: false,
            status,
            revision: 1,
            generation: 0,
            acp_session_id: acp_session_id.map(str::to_owned),
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

    #[test]
    fn extract_report_accepts_embedded_json() {
        let text = "some intro\n".to_owned()
            + &json!({
                "status": "completed",
                "summary": "s",
                "changed_files": [],
                "checks": [],
                "unresolved": [],
            })
            .to_string();
        let (report, error) = extract_report(&text);
        assert!(error.is_none());
        assert_eq!(report.unwrap()["summary"], "s");
    }

    #[test]
    fn extract_report_rejects_invalid() {
        let (report, error) = extract_report("no json here");
        assert!(report.is_none());
        assert!(error.is_some());
    }

    #[test]
    fn parse_initialize_response_requires_protocol_version() {
        assert!(parse_initialize_response(&json!({})).is_err());
        let caps = parse_initialize_response(&json!({
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": true},
        }))
        .unwrap();
        assert!(caps.load_session);
    }

    #[test]
    fn permission_option_prefers_allow_once() {
        let params = json!({
            "options": [
                {"optionId": "reject1", "kind": "reject_once"},
                {"optionId": "allow1", "kind": "allow_once"},
            ]
        });
        assert_eq!(select_permission_option(&params).as_deref(), Some("allow1"));
        assert!(select_permission_option(&json!({"options": []})).is_none());
    }
}
