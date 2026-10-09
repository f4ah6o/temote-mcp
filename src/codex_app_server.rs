use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::orchestration::outcome::{self, DeliveryRecord, VerificationRecord};
use crate::pending_interaction::{
    InteractionType, PendingInteractionSummary, ProducerKind, REFRESH_INTERVAL_SECS, Summary,
    SummaryState,
};
use crate::{approvals, config, evidence};

const APP_SERVER_CLIENT_NAME: &str = "temote-mcp";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CodexContinuation {
    #[default]
    New,
    PreviousTask {
        task_id: Uuid,
    },
}

pub(crate) fn codex_continuation(args: &Value) -> Result<CodexContinuation> {
    match args.get("continuation") {
        None => Ok(CodexContinuation::New),
        Some(value) => {
            let object = value
                .as_object()
                .context("continuation must be an object")?;
            let exact = match object.get("type").and_then(Value::as_str) {
                Some("new") => object.len() == 1,
                Some("previous_task") => object.len() == 2 && object.contains_key("task_id"),
                _ => false,
            };
            anyhow::ensure!(
                exact,
                "{}",
                "continuation must be {type:new} or {type:previous_task,task_id:UUID}"
            );
            serde_json::from_value(value.clone())
                .context("continuation must be {type:new} or {type:previous_task,task_id:UUID}")
        }
    }
}
const APP_SERVER_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_APP_SERVER_USER_AGENT_BYTES: usize = 512;
const TASK_SCHEMA_VERSION: u64 = 1;
const TASK_RETENTION_SECONDS: u64 = 24 * 60 * 60;
const MAX_TASK_RECORD_BYTES: usize = 64 * 1024;
const MAX_TASK_DIRECTORY_ENTRIES: usize = 4096;
const MAX_TASKS_PER_SCOPE: usize = 128;
const MAX_OPERATION_HISTORY: usize = 128;
const CODEX_APPROVAL_POLICY: &str = "on-request";
const MAX_TASK_INPUT_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 256;
const MAX_RPC_LINE_BYTES: usize = 4 * 1024 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const SESSION_STOP_POLL: Duration = Duration::from_secs(1);
const SESSION_CODEX_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_USAGE_FIELDS: &[&str] = &[
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "total_tokens",
];

const TASK_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x35, 0x70, 0xb9, 0xde, 0x9e, 0x41, 0x47, 0x89, 0xb8, 0x23, 0x28, 0x07, 0x54, 0x96, 0x11, 0xa4,
]);
const REQUEST_FINGERPRINT_NAMESPACE: Uuid = Uuid::from_bytes([
    0xa5, 0x5f, 0x89, 0x38, 0x32, 0x5c, 0x44, 0xc7, 0x87, 0x9e, 0x24, 0x7a, 0x88, 0x65, 0xa5, 0x5c,
]);

const CODEX_CHILD_ENV_ALLOWLIST: &[&str] = &[
    "ALL_PROXY",
    "CODEX_HOME",
    "HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "LANG",
    "LOGNAME",
    "NO_PROXY",
    "OPENAI_API_KEY",
    "PATH",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "USER",
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
struct SessionInstance {
    id: String,
    started_at: u64,
    process_id: u32,
}

struct CodexLifecycleEntry {
    closing: bool,
    cleanup_complete: bool,
    in_flight: usize,
    task_controls: HashMap<(PathBuf, Uuid), usize>,
    cancellation: watch::Sender<bool>,
    drain_notify: Arc<tokio::sync::Notify>,
}

impl CodexLifecycleEntry {
    fn new() -> Self {
        let (cancellation, _) = watch::channel(false);
        Self {
            closing: false,
            cleanup_complete: false,
            in_flight: 0,
            task_controls: HashMap::new(),
            cancellation,
            drain_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

#[derive(Default)]
struct CodexLifecycleRegistry {
    entries: HashMap<SessionInstance, CodexLifecycleEntry>,
}

fn codex_lifecycle_registry() -> &'static Mutex<CodexLifecycleRegistry> {
    static REGISTRY: OnceLock<Mutex<CodexLifecycleRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(CodexLifecycleRegistry::default()))
}

pub(crate) fn begin_session_shutdown(session: &config::Session) {
    begin_session_instance_shutdown(&SessionInstance::from_session(session));
}

fn begin_session_instance_shutdown(owner: &SessionInstance) {
    let mut registry = codex_lifecycle_registry().lock().unwrap();
    let entry = registry
        .entries
        .entry(owner.clone())
        .or_insert_with(CodexLifecycleEntry::new);
    if !entry.closing {
        entry.closing = true;
        let _ = entry.cancellation.send(true);
    }
}

fn finish_session_shutdown(owner: &SessionInstance) -> Result<()> {
    let mut registry = codex_lifecycle_registry().lock().unwrap();
    let Some(entry) = registry.entries.get_mut(owner) else {
        return Ok(());
    };
    anyhow::ensure!(entry.closing, "Codex session instance is not closing");
    anyhow::ensure!(
        entry.in_flight == 0,
        "Codex session shutdown finished with {} in-flight operation(s)",
        entry.in_flight
    );
    entry.cleanup_complete = true;
    registry.entries.remove(owner);
    Ok(())
}

// Only live control calls defer reads. Retained accepted receipts must still
// reconcile after a caller or backend crashes.
struct TaskControlGuard {
    owner: SessionInstance,
    key: (PathBuf, Uuid),
}

impl TaskControlGuard {
    fn acquire(session: &config::Session, task_id: Uuid) -> Result<Self> {
        let owner = SessionInstance::from_session(session);
        let key = (config::canonical_directory(&session.cwd)?, task_id);
        let mut registry = codex_lifecycle_registry().lock().unwrap();
        let entry = registry
            .entries
            .get_mut(&owner)
            .context("Codex session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "Codex session instance is closing");
        *entry.task_controls.entry(key.clone()).or_default() += 1;
        Ok(Self { owner, key })
    }
}

impl Drop for TaskControlGuard {
    fn drop(&mut self) {
        let mut registry = codex_lifecycle_registry().lock().unwrap();
        let Some(entry) = registry.entries.get_mut(&self.owner) else {
            return;
        };
        if let Some(count) = entry.task_controls.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                entry.task_controls.remove(&self.key);
            }
        }
    }
}

fn task_control_in_flight(record: &TaskRecord) -> bool {
    codex_lifecycle_registry()
        .lock()
        .unwrap()
        .entries
        .get(&record.owner)
        .is_some_and(|entry| {
            entry
                .task_controls
                .contains_key(&(record.scope_cwd.clone(), record.task_id))
        })
}

fn session_instance_is_closing(owner: &SessionInstance) -> bool {
    codex_lifecycle_registry()
        .lock()
        .unwrap()
        .entries
        .get(owner)
        .is_some_and(|entry| entry.closing)
}

struct CodexLifecyclePermit {
    owner: SessionInstance,
    cancellation: watch::Receiver<bool>,
}

impl Drop for CodexLifecyclePermit {
    fn drop(&mut self) {
        let (drain_notify, should_remove) = {
            let mut registry = codex_lifecycle_registry().lock().unwrap();
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
            let mut registry = codex_lifecycle_registry().lock().unwrap();
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
                let registry = codex_lifecycle_registry().lock().unwrap();
                let Some(entry) = registry.entries.get(owner) else {
                    return Ok::<(), anyhow::Error>(());
                };
                anyhow::ensure!(
                    entry.closing,
                    "cannot drain Codex operations for a session instance that is not closing"
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
                "Codex session shutdown timed out waiting for in-flight operations to drain (session {})",
                owner.id
            )
        })??;
    Ok(())
}

pub(crate) fn ensure_session_replacement_allowed(session_id: &str) -> Result<()> {
    let registry = codex_lifecycle_registry().lock().unwrap();
    anyhow::ensure!(
        !registry.entries.keys().any(|owner| owner.id == session_id),
        "Codex session {session_id} is still draining its previous instance"
    );
    Ok(())
}

async fn ensure_current_active_instance(
    owner: &SessionInstance,
    session: &config::Session,
) -> Result<CodexLifecyclePermit> {
    anyhow::ensure!(
        owner.matches(session),
        "Codex operation session snapshot does not match its owner instance"
    );
    anyhow::ensure!(
        !session_instance_is_closing(owner),
        "Codex session instance is closing"
    );
    let current = config::read_session_metadata(&owner.id)
        .await
        .with_context(|| format!("cannot verify current Codex session instance {}", owner.id))?;
    anyhow::ensure!(
        owner.matches(&current),
        "Codex session instance is no longer current"
    );
    anyhow::ensure!(
        config::session_is_active(&owner.id).await?,
        "Codex session instance is not active"
    );

    let mut registry = codex_lifecycle_registry().lock().unwrap();
    let entry = registry
        .entries
        .entry(owner.clone())
        .or_insert_with(CodexLifecycleEntry::new);
    anyhow::ensure!(
        !entry.closing,
        "Codex session instance began closing while it was being verified"
    );
    entry.in_flight += 1;
    Ok(CodexLifecyclePermit {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionMonitorUnknownKind {
    MetadataRead,
    ActivityProbe,
}

impl SessionMonitorUnknownKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::MetadataRead => "metadata_read",
            Self::ActivityProbe => "activity_probe",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionMonitorObservation {
    Active,
    Inactive,
    OwnerChanged,
    Unknown(SessionMonitorUnknownKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionMonitorStopReason {
    Inactive,
    OwnerChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionMonitorAction {
    Continue,
    Stop(SessionMonitorStopReason),
}

#[derive(Default)]
struct SessionStopMonitor {
    consecutive_unknown: u8,
}

impl SessionStopMonitor {
    fn observe(&mut self, observation: SessionMonitorObservation) -> SessionMonitorAction {
        match observation {
            SessionMonitorObservation::Active => {
                self.consecutive_unknown = 0;
                SessionMonitorAction::Continue
            }
            SessionMonitorObservation::Inactive => {
                SessionMonitorAction::Stop(SessionMonitorStopReason::Inactive)
            }
            SessionMonitorObservation::OwnerChanged => {
                SessionMonitorAction::Stop(SessionMonitorStopReason::OwnerChanged)
            }
            SessionMonitorObservation::Unknown(kind) => {
                let _ = kind;
                self.consecutive_unknown = self.consecutive_unknown.saturating_add(1);
                SessionMonitorAction::Continue
            }
        }
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
    thread_id: Option<String>,
    turn_id: Option<String>,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct TaskRecord {
    schema_version: u64,
    task_id: Uuid,
    owner: SessionInstance,
    scope_cwd: PathBuf,
    model: String,
    effort: String,
    status: TaskStatus,
    revision: u64,
    generation: u64,
    thread_id: Option<String>,
    #[serde(default)]
    continued_from_task_id: Option<Uuid>,
    #[serde(default)]
    continued_by_task_id: Option<Uuid>,
    #[serde(default)]
    continued_by_request_fingerprint: Option<Uuid>,
    turn_id: Option<String>,
    #[serde(default)]
    previous_turn_id: Option<String>,
    #[serde(default)]
    usage: Option<BTreeMap<String, u64>>,
    #[serde(default)]
    report: Option<Value>,
    #[serde(default)]
    report_source: Option<String>,
    #[serde(default)]
    report_status: Option<String>,
    #[serde(default)]
    pending_interaction: Option<PendingInteractionSummary>,
    created_at: u64,
    updated_at: u64,
    #[serde(default)]
    verification: Option<VerificationRecord>,
    #[serde(default)]
    delivery: Option<DeliveryRecord>,
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
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
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
    Accepted(
        TaskRecord,
        TaskRuntimeLease,
        Option<RuntimeHandle>,
        Option<String>,
    ),
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

fn set_pending_summary(record: &mut TaskRecord, summary: Summary) {
    // The summary has its own cursor. Runtime-owner observations must not
    // invalidate verification tied to the task's semantic revision.
    record.pending_interaction = Some(summary);
}

impl TaskStore {
    fn default_store() -> Result<Self> {
        Ok(Self {
            directory: config::state_dir()?.join("codex-tasks"),
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
            Err(error) => Err(error).context("cannot inspect Codex task store"),
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
                    .context("cannot lock Codex task store");
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
                return Err(error).context("cannot lock Codex task runtime");
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
            Err(error).context("cannot inspect Codex task runtime lock")
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

    #[cfg(test)]
    fn load(&self, session: &config::Session, task_id: Uuid) -> Result<TaskRecord> {
        let _guard = self.lock()?;
        self.load_locked(session, task_id)
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
                anyhow::anyhow!("CODEX_TASK_NOT_FOUND: task was not found")
            } else {
                error
            }
        })?;
        ensure_task_owner(&record, session)?;
        Ok(record)
    }

    #[cfg(test)]
    fn save(&self, record: &TaskRecord) -> Result<()> {
        let _guard = self.lock()?;
        self.save_locked(record)
    }

    fn save_locked(&self, record: &TaskRecord) -> Result<()> {
        self.ensure_directory()?;
        validate_record(record)?;
        self.prune_locked(record)?;
        self.write_record_locked(record)
    }

    /// Persist runtime-owned summary metadata without pruning other records or
    /// extending task retention.
    fn save_metadata_locked(&self, record: &TaskRecord) -> Result<()> {
        self.ensure_directory()?;
        validate_record(record)?;
        self.write_record_locked(record)
    }

    fn write_record_locked(&self, record: &TaskRecord) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(record)?;
        anyhow::ensure!(
            bytes.len() <= MAX_TASK_RECORD_BYTES,
            "Codex task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
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
        let previous = record.clone();
        f(&mut record)?;
        if record == previous {
            return Ok(record);
        }
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
        let registry = codex_lifecycle_registry().lock().unwrap();
        let entry = registry
            .entries
            .get(owner)
            .context("Codex session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "Codex session instance is closing");
        let mut record = self.load_locked(session, task_id)?;
        let previous = record.clone();
        f(&mut record)?;
        if record != previous {
            record.updated_at = config::unix_time();
            self.save_locked(&record)?;
        }
        Ok(record)
    }

    /// Persist the bounded projection of approvals known by the currently
    /// connected runtime. This deliberately avoids `update`: summary
    /// heartbeats must not extend task retention or change task revisions.
    fn observe_pending_interaction(
        &self,
        session: &config::Session,
        task_id: Uuid,
        runtime_instance_id: Uuid,
        expected_generation: Option<u64>,
    ) -> Result<bool> {
        if !runtime_registration_matches(session, task_id, runtime_instance_id) {
            return Ok(false);
        }
        let client = {
            let registered = runtimes().lock().unwrap();
            let Some(runtime) = registered.get(&task_id) else {
                return Ok(false);
            };
            if runtime.instance_id != runtime_instance_id {
                return Ok(false);
            }
            runtime.client.clone()
        };
        if !client.is_connected() {
            return Ok(false);
        }
        let pending_binding = client.pending_summary_binding();

        let _guard = self.lock()?;
        if !runtime_registration_matches(session, task_id, runtime_instance_id) {
            return Ok(false);
        }
        if !client.is_connected() {
            return Ok(false);
        }

        let mut record = self.load_locked(session, task_id)?;
        let now = config::unix_time();
        if record.status.is_terminal()
            || now.saturating_sub(record.updated_at) >= TASK_RETENTION_SECONDS
            || expected_generation.is_some_and(|generation| generation != record.generation)
        {
            return Ok(false);
        }
        let pending_count = client.pending_approvals.load(Ordering::Acquire);
        let observed_at = config::unix_time();
        let pending_state_ready = pending_binding.as_ref().is_some_and(|binding| {
            record.generation > 0
                && record.thread_id.as_deref() == Some(binding.thread_id.as_str())
                && record.turn_id.as_deref() == Some(binding.turn_id.as_str())
                && record.generation == binding.generation
        });
        if record.generation == 0 {
            return Ok(true);
        }

        if pending_count == 0
            && (!pending_state_ready || record.status == TaskStatus::WaitingApproval)
        {
            let summary = Summary::unavailable(
                record.pending_interaction.as_ref(),
                ProducerKind::RuntimeOwner,
                record.generation,
            )?;
            set_pending_summary(&mut record, summary);
            if !runtime_registration_matches(session, task_id, runtime_instance_id) {
                return Ok(false);
            }
            self.save_metadata_locked(&record)?;
            return Ok(true);
        }

        let count = pending_count.min(crate::pending_interaction::MAX_COUNT as u64) as u8;
        let state = if pending_count == 0 {
            SummaryState::None
        } else if pending_count > crate::pending_interaction::MAX_COUNT as u64 {
            SummaryState::Unavailable
        } else {
            SummaryState::Pending
        };
        let types = if pending_count == 0 {
            Vec::new()
        } else {
            vec![InteractionType::Approval]
        };
        let summary = Summary::observe(
            record.pending_interaction.as_ref(),
            state,
            Some(count),
            &types,
            pending_count > crate::pending_interaction::MAX_COUNT as u64,
            ProducerKind::RuntimeOwner,
            record.generation,
            observed_at,
        )?;
        set_pending_summary(&mut record, summary);
        if !runtime_registration_matches(session, task_id, runtime_instance_id) {
            return Ok(false);
        }
        self.save_metadata_locked(&record)?;
        Ok(true)
    }

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
        let registry = codex_lifecycle_registry().lock().unwrap();
        let entry = registry
            .entries
            .get(owner)
            .context("Codex session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "Codex session instance is closing");
        self.accept_start_locked(session, record)
    }

    /// A crash can leave A's claim persisted before B's record is written.
    /// Search under the store lock so a retry cannot redirect B's operation ID
    /// to another source while that durable claim exists.
    fn claim_for_successor_locked(&self, successor_id: Uuid) -> Result<Option<(Uuid, Uuid)>> {
        let mut claim = None;
        let mut count = 0usize;
        for entry in std::fs::read_dir(&self.directory)
            .context("cannot inspect Codex continuation claims")?
        {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "Codex task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
            );
            let name = entry?.file_name();
            let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            let Ok(task_id) = Uuid::parse_str(stem) else {
                continue;
            };
            let record = self.read_record(task_id).with_context(|| {
                format!("cannot inspect Codex continuation claim in task {task_id}")
            })?;
            if record.continued_by_task_id == Some(successor_id) {
                let fingerprint = record
                    .continued_by_request_fingerprint
                    .context("Codex continuation claim is incomplete")?;
                anyhow::ensure!(
                    claim.is_none(),
                    "CODEX_CONTINUATION_CONFLICT: multiple sources claim the same successor"
                );
                claim = Some((task_id, fingerprint));
            }
        }
        Ok(claim)
    }

    fn accept_start_locked(
        &self,
        session: &config::Session,
        record: TaskRecord,
    ) -> Result<StartAcceptance> {
        let candidate_receipt = record
            .operations
            .first()
            .context("Codex start acceptance is missing its operation receipt")?;
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
                            && existing.thread_id.is_none()
                    });
                if retryable {
                    let resume_thread = if let Some(source_id) = existing.continued_from_task_id {
                        let source = self.load_locked(session, source_id).context("CODEX_CONTINUATION_SOURCE_UNAVAILABLE: source must be a retained Codex task in this exact session and scope")?;
                        anyhow::ensure!(
                            source.continued_by_task_id == Some(existing.task_id),
                            "CODEX_CONTINUATION_CLAIM_LOST: source belongs to another successor"
                        );
                        anyhow::ensure!(
                            source.continued_by_request_fingerprint
                                == Some(candidate_receipt.request_fingerprint),
                            "CODEX_CONTINUATION_CLAIM_LOST: source claim does not match successor request"
                        );
                        Some(source.thread_id.context(
                            "CODEX_CONTINUATION_UNAVAILABLE: source has no retained thread",
                        )?)
                    } else {
                        None
                    };
                    let lease = self
                        .try_acquire_runtime_lease_locked(existing.task_id)?
                        .context("CODEX_TASK_RUNTIME_OWNED: task runtime belongs to another Temote process")?;
                    existing.status = TaskStatus::Accepted;
                    existing.revision = existing.revision.saturating_add(1);
                    update_operation_receipt(
                        &mut existing,
                        candidate_receipt.operation_id,
                        OperationPhase::Accepted,
                    );
                    existing.updated_at = config::unix_time();
                    self.save_locked(&existing)?;
                    Ok(StartAcceptance::Accepted(
                        existing,
                        lease,
                        None,
                        resume_thread,
                    ))
                } else {
                    Ok(StartAcceptance::Existing(existing))
                }
            }
            Err(error) if is_not_found(&error) => {
                if let Some((claimed_source, fingerprint)) =
                    self.claim_for_successor_locked(record.task_id)?
                {
                    anyhow::ensure!(
                        record.continued_from_task_id == Some(claimed_source)
                            && candidate_receipt.request_fingerprint == fingerprint,
                        "OPERATION_CONFLICT: operation_id was already claimed for a different continuation request"
                    );
                }
                let (mut source, resume_thread, new_claim) = if let Some(source_id) =
                    record.continued_from_task_id
                {
                    anyhow::ensure!(
                        source_id != record.task_id,
                        "CODEX_CONTINUATION_INVALID: task cannot continue itself"
                    );
                    let mut source = self.load_locked(session, source_id).context("CODEX_CONTINUATION_SOURCE_UNAVAILABLE: source must be a retained Codex task in this exact session and scope")?;
                    anyhow::ensure!(
                        source.status.is_terminal()
                            && !source
                                .operations
                                .iter()
                                .any(|receipt| receipt.phase == OperationPhase::Accepted),
                        "CODEX_CONTINUATION_NOT_QUIESCENT: source task has an active turn or control"
                    );
                    let new_claim = source.continued_by_task_id.is_none();
                    match source.continued_by_task_id {
                        None => {}
                        Some(successor) if successor == record.task_id => {
                            anyhow::ensure!(
                                source.continued_by_request_fingerprint
                                    == Some(candidate_receipt.request_fingerprint),
                                "OPERATION_CONFLICT: operation_id was already accepted with a different continuation request"
                            );
                        }
                        Some(_) => anyhow::bail!(
                            "CODEX_CONTINUATION_CLAIMED: source conversation already has a successor"
                        ),
                    }
                    let thread = source
                        .thread_id
                        .clone()
                        .filter(|id| !id.is_empty())
                        .context("CODEX_CONTINUATION_UNAVAILABLE: source has no retained thread")?;
                    {
                        let state = runtimes().lock().unwrap();
                        if let Some(runtime) = state.get(&source_id) {
                            anyhow::ensure!(
                                runtime.owner == source.owner && runtime.scope == source.scope_cwd,
                                "CODEX_CONTINUATION_RUNTIME_MISMATCH: source runtime does not match record"
                            );
                        } else {
                            anyhow::ensure!(
                                !self.runtime_lease_held_locked(source_id)?,
                                "CODEX_CONTINUATION_RUNTIME_OWNED: source runtime belongs to another process"
                            );
                        }
                    }
                    source.continued_by_task_id = Some(record.task_id);
                    source.continued_by_request_fingerprint =
                        Some(candidate_receipt.request_fingerprint);
                    source.updated_at = config::unix_time();
                    (Some(source), Some(thread), new_claim)
                } else {
                    (None, None, false)
                };
                let lease = self
                    .try_acquire_runtime_lease_locked(record.task_id)?
                    .context(
                        "CODEX_TASK_RUNTIME_OWNED: task runtime belongs to another Temote process",
                    )?;
                if let Some(source_record) = source.as_ref().filter(|_| new_claim) {
                    self.save_locked(source_record)?;
                }
                if let Err(error) = self.save_locked(&record) {
                    if let Some(mut source_record) = source.take().filter(|_| new_claim) {
                        source_record.continued_by_task_id = None;
                        source_record.continued_by_request_fingerprint = None;
                        self.save_locked(&source_record)?;
                    }
                    drop(lease);
                    match std::fs::remove_file(self.runtime_lock_path(record.task_id)) {
                        Ok(()) => {}
                        Err(cleanup_error)
                            if cleanup_error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(cleanup_error) => {
                            return Err(error).context(format!(
                                "cannot remove unused Codex runtime lock: {cleanup_error}"
                            ));
                        }
                    }
                    return Err(error);
                }
                let retired_runtime = record
                    .continued_from_task_id
                    .and_then(|source_id| runtimes().lock().unwrap().remove(&source_id));
                Ok(StartAcceptance::Accepted(
                    record,
                    lease,
                    retired_runtime,
                    resume_thread,
                ))
            }
            Err(error) => Err(error),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn accept_control(
        &self,
        session: &config::Session,
        task_id: Uuid,
        operation_id: Uuid,
        request_fingerprint: Uuid,
        action: &str,
    ) -> Result<ControlAcceptance> {
        let _guard = self.lock()?;
        self.accept_control_locked(session, task_id, operation_id, request_fingerprint, action)
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
        let registry = codex_lifecycle_registry().lock().unwrap();
        let owner = SessionInstance::from_session(session);
        let entry = registry
            .entries
            .get(&owner)
            .context("Codex session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "Codex session instance is closing");
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
        anyhow::ensure!(
            record.continued_by_task_id.is_none(),
            "CODEX_CONTINUATION_CLAIMED: source conversation belongs to its successor"
        );
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

        let completed_steer = action == "steer"
            && matches!(
                record.status,
                TaskStatus::Completed | TaskStatus::Interrupted
            );
        if action == "resume" {
            anyhow::ensure!(
                matches!(
                    record.status,
                    TaskStatus::Unknown | TaskStatus::ReconciliationRequired
                ),
                "Codex task does not require resume reconciliation"
            );
        } else if !completed_steer {
            anyhow::ensure!(
                matches!(
                    record.status,
                    TaskStatus::Running | TaskStatus::WaitingApproval
                ),
                "Codex task is not active and cannot be controlled"
            );
        }
        anyhow::ensure!(
            record.thread_id.is_some(),
            "Codex task requires reconciliation before control"
        );
        if action != "resume" {
            anyhow::ensure!(
                record.turn_id.is_some(),
                "Codex task requires reconciliation before control"
            );
        }

        let runtime_lease = if runtime_matches_record(&record) {
            None
        } else {
            Some(self.try_acquire_runtime_lease_locked(task_id)?.context(
                "CODEX_TASK_RUNTIME_OWNED: task runtime belongs to another Temote process",
            )?)
        };

        if completed_steer {
            record.previous_turn_id = record.turn_id.take();
            record.status = TaskStatus::ReconciliationRequired;
            record.generation = record.generation.saturating_add(1);
            record.report = None;
            record.report_source = None;
            record.report_status = None;
        }

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
            "Codex task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_TASK_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= MAX_TASK_RECORD_BYTES,
            "Codex task record exceeds {MAX_TASK_RECORD_BYTES} bytes"
        );
        let record: TaskRecord =
            serde_json::from_slice(&bytes).context("invalid Codex task record")?;
        validate_record(&record)?;
        anyhow::ensure!(record.task_id == task_id, "Codex task record ID mismatch");
        Ok(record)
    }

    fn prune_locked(&self, current: &TaskRecord) -> Result<()> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("cannot list Codex task store"),
        };
        let now = config::unix_time();
        let mut scoped = Vec::new();
        let mut count = 0usize;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "Codex task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
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
                    .with_context(|| format!("cannot prune expired Codex task {id}"))?;
                match std::fs::remove_file(self.runtime_lock_path(id)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("cannot prune Codex runtime lock {id}"));
                    }
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
                "Codex task scope has reached its retention limit; refusing to accept another task"
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
            Err(error) => return Err(error).context("cannot inspect Codex task store"),
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
            Err(error) => return Err(error).context("cannot list Codex task store"),
        };
        let now = config::unix_time();
        let mut count = 0usize;
        let mut finalized = 0usize;
        let mut deferred = false;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "Codex task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
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
                        .with_context(|| format!("cannot inspect Codex task {id} during cleanup"));
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

    /// Read-only projection of every record owned by `session`'s full
    /// instance and canonical scope. Unreadable record files count as
    /// `skipped` so a partial projection stays visible; a missing store
    /// directory is an empty list, not an error.
    fn list_owned(&self, session: &config::Session) -> Result<(Vec<TaskRecord>, usize)> {
        let _guard = self.lock()?;
        let scope = config::canonical_directory(&session.cwd)?;
        let metadata = match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), 0));
            }
            Err(error) => return Err(error).context("cannot inspect Codex task store"),
        };
        validate_store_directory(&self.directory, &metadata)?;
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), 0));
            }
            Err(error) => return Err(error).context("cannot list Codex task store"),
        };
        let mut records = Vec::new();
        let mut skipped = 0usize;
        let mut count = 0usize;
        for entry in entries {
            count += 1;
            anyhow::ensure!(
                count <= MAX_TASK_DIRECTORY_ENTRIES,
                "Codex task store contains more than {MAX_TASK_DIRECTORY_ENTRIES} entries"
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
            match self.read_record(id) {
                Ok(record) => {
                    if record.owner.matches(session) && record.scope_cwd == scope {
                        records.push(record);
                    }
                }
                Err(error) if is_not_found(&error) => continue,
                Err(_) => skipped += 1,
            }
        }
        Ok((records, skipped))
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
        "Codex task store must be a real directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "Codex task store must be owner-only (mode {mode:04o})"
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
        "Codex task path is not a regular file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(mode & 0o077 == 0, "Codex task file must be owner-only");
    }
    Ok(())
}

fn reject_symlink_target(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "Codex task path may not be a symlink"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect Codex task path"),
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

#[cfg(test)]
fn is_missing_task(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().starts_with("CODEX_TASK_NOT_FOUND:"))
}

fn ensure_task_owner(record: &TaskRecord, session: &config::Session) -> Result<()> {
    let scope = config::canonical_directory(&session.cwd)?;
    anyhow::ensure!(
        record.owner.matches(session) && record.scope_cwd == scope,
        "CODEX_TASK_NOT_FOUND: task was not found"
    );
    Ok(())
}

fn validate_record(record: &TaskRecord) -> Result<()> {
    anyhow::ensure!(
        record.schema_version == TASK_SCHEMA_VERSION,
        "unsupported Codex task schema version"
    );
    config::validate_session_id(&record.owner.id)?;
    let canonical = config::canonical_directory(&record.scope_cwd)?;
    anyhow::ensure!(
        canonical == record.scope_cwd,
        "Codex task scope is not canonical"
    );
    anyhow::ensure!(
        record.continued_by_task_id.is_some() == record.continued_by_request_fingerprint.is_some(),
        "Codex continuation claim is incomplete"
    );
    anyhow::ensure!(
        record.continued_from_task_id != Some(record.task_id)
            && record.continued_by_task_id != Some(record.task_id),
        "Codex task cannot continue itself"
    );
    if let Some(verification) = &record.verification {
        verification.validate()?;
    }
    if let Some(delivery) = &record.delivery {
        delivery.validate()?;
    }
    validate_argument(&record.model, "model")?;
    validate_argument(&record.effort, "effort")?;
    anyhow::ensure!(record.revision > 0, "Codex task revision must be positive");
    anyhow::ensure!(
        record.operations.len() <= MAX_OPERATION_HISTORY,
        "Codex task operation history exceeds limit"
    );
    let mut operation_ids = record
        .operations
        .iter()
        .map(|receipt| receipt.operation_id)
        .collect::<std::collections::BTreeSet<_>>();
    for tombstone in &record.operation_tombstones {
        anyhow::ensure!(
            operation_ids.insert(tombstone.operation_id),
            "Codex task operation history contains a duplicate operation_id"
        );
    }
    if let Some(usage) = &record.usage {
        anyhow::ensure!(
            usage.len() <= TOKEN_USAGE_FIELDS.len()
                && usage
                    .keys()
                    .all(|key| TOKEN_USAGE_FIELDS.contains(&key.as_str())),
            "Codex task usage has unsupported fields"
        );
    }
    if let Some(summary) = &record.pending_interaction {
        summary.validate()?;
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
    bytes.extend_from_slice(scope.as_os_str().as_bytes());
}

#[cfg(not(unix))]
fn append_scope_identity(bytes: &mut Vec<u8>, scope: &Path) {
    bytes.extend_from_slice(scope.to_string_lossy().as_bytes());
}

pub(crate) fn task_id_for_operation(session: &config::Session, operation_id: Uuid) -> Result<Uuid> {
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

/// Internal authority that selected a Codex task start.
///
/// Public `codex_task_start` input can only select `Generic`. Higher-level
/// tools construct their origin inside Temote so prompt text cannot impersonate
/// a typed operation. Clone request values are committed only through the
/// durable fingerprint; the receipt does not persist the original strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TaskStartOrigin {
    Generic,
    OpenCodeWorkspaceCheck {
        parent_task_id: Uuid,
        workspace_id: Uuid,
        action: String,
        producer_epoch: u64,
    },
    RepositoryCloneBare {
        root: String,
        source: String,
        destination: String,
    },
}

impl TaskStartOrigin {
    pub(crate) fn opencode_workspace_check(
        parent_task_id: Uuid,
        workspace_id: Uuid,
        action: &str,
        producer_epoch: u64,
    ) -> Self {
        Self::OpenCodeWorkspaceCheck {
            parent_task_id,
            workspace_id,
            action: action.to_owned(),
            producer_epoch,
        }
    }
    pub(crate) fn repository_clone_bare(root: &str, source: &str, destination: &str) -> Self {
        Self::RepositoryCloneBare {
            root: root.to_owned(),
            source: source.to_owned(),
            destination: destination.to_owned(),
        }
    }
}

fn task_start_fingerprint(
    task_id: Uuid,
    task: &str,
    model: &str,
    effort: &str,
    origin: &TaskStartOrigin,
) -> Result<Uuid> {
    match origin {
        // Preserve the exact legacy generic fingerprint so retained normal
        // Codex tasks remain replayable across this change.
        TaskStartOrigin::Generic => fingerprint(&json!({
            "kind": "start",
            "task_id": task_id,
            "task": task,
            "model": model,
            "effort": effort,
        })),
        TaskStartOrigin::OpenCodeWorkspaceCheck {
            parent_task_id,
            workspace_id,
            action,
            producer_epoch,
        } => fingerprint(&json!({
            "kind": "start",
            "task_id": task_id,
            "task": task,
            "model": model,
            "effort": effort,
            "origin": {
                "kind": "opencode_workspace_check",
                "parent_task_id": parent_task_id,
                "workspace_id": workspace_id,
                "action": action,
                "producer_epoch": producer_epoch,
            }
        })),
        TaskStartOrigin::RepositoryCloneBare {
            root,
            source,
            destination,
        } => fingerprint(&json!({
            "kind": "start",
            "task_id": task_id,
            "task": task,
            "model": model,
            "effort": effort,
            "origin": {
                "kind": "repository_clone_bare",
                "root": root,
                "source": source,
                "destination": destination,
            },
        })),
    }
}

fn continuation_fingerprint(
    task_id: Uuid,
    task: &str,
    model: &str,
    effort: &str,
    origin: &TaskStartOrigin,
    continuation: CodexContinuation,
) -> Result<Uuid> {
    match continuation {
        CodexContinuation::New => task_start_fingerprint(task_id, task, model, effort, origin),
        CodexContinuation::PreviousTask { task_id: source } => {
            anyhow::ensure!(
                matches!(origin, TaskStartOrigin::Generic),
                "continuation is only supported for codex_task_start"
            );
            fingerprint(&json!({
                "kind": "start", "task_id": task_id, "task": task,
                "model": model, "effort": effort,
                "continuation": {"type": "previous_task", "task_id": source},
            }))
        }
    }
}

fn task_view(record: &TaskRecord, evidence_ref: Option<&evidence::EvidenceRef>) -> Value {
    json!({
        "task_id": record.task_id,
        "status": record.status.as_str(),
        "revision": record.revision,
        "generation": record.generation,
        "execution": outcome::execution_view(record.task_id, record.generation, record.status.as_str()),
        "verification": outcome::verification_view(record.verification.as_ref(), record.revision),
        "delivery": outcome::delivery_view(record.delivery.as_ref()),
        "model": record.model,
        "effort": record.effort,
        "thread_id": record.thread_id,
        "continued_from_task_id": record.continued_from_task_id,
        "continued_by_task_id": record.continued_by_task_id,
        "turn_id": record.turn_id,
        "previous_turn_id": record.previous_turn_id,
        "usage": record.usage,
        "report": record.report,
        "report_source": record.report_source,
        "report_status": record.report_status,
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

fn reconciled_task_view(
    record: &TaskRecord,
    evidence_ref: Option<&evidence::EvidenceRef>,
    after_revision: Option<u64>,
) -> Value {
    if after_revision == Some(record.revision) {
        return json!({
            "task_id": record.task_id,
            "status": "not_modified",
            "revision": record.revision,
        });
    }
    task_view(record, evidence_ref)
}

fn store_evidence_for_instance(
    owner: &SessionInstance,
    session: &config::Session,
    response: &Value,
) -> Option<evidence::EvidenceRef> {
    let registry = codex_lifecycle_registry().lock().unwrap();
    if registry
        .entries
        .get(owner)
        .is_none_or(|entry| entry.closing)
    {
        return None;
    }
    evidence::store_for_session(session, serde_json::to_string(response).ok()?)
        .ok()
        .flatten()
}

fn apply_thread_start_response(
    store: &TaskStore,
    session: &config::Session,
    task_id: Uuid,
    thread_id: &str,
) -> Result<TaskRecord> {
    store.update(session, task_id, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        anyhow::ensure!(record.thread_id.is_none(), "task thread was already bound");
        record.thread_id = Some(thread_id.to_owned());
        record.revision = record.revision.saturating_add(1);
        Ok(())
    })
}

fn apply_thread_start_response_for_instance(
    store: &TaskStore,
    session: &config::Session,
    task_id: Uuid,
    thread_id: &str,
    owner: &SessionInstance,
) -> Result<TaskRecord> {
    store.update_if_instance_live(session, task_id, owner, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        anyhow::ensure!(record.thread_id.is_none(), "task thread was already bound");
        record.thread_id = Some(thread_id.to_owned());
        record.revision = record.revision.saturating_add(1);
        Ok(())
    })
}

fn apply_turn_start_response(
    store: &TaskStore,
    session: &config::Session,
    task_id: Uuid,
    thread_id: &str,
    turn_id: &str,
    operation_id: Uuid,
) -> Result<TaskRecord> {
    store.update(session, task_id, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        anyhow::ensure!(
            record.thread_id.as_deref() == Some(thread_id),
            "turn/start returned for an unexpected task thread"
        );
        if let Some(existing) = record.turn_id.as_deref() {
            anyhow::ensure!(
                existing == turn_id,
                "turn/start returned an unexpected turn id"
            );
        } else {
            record.turn_id = Some(turn_id.to_owned());
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
    })
}

fn apply_turn_start_response_for_instance(
    store: &TaskStore,
    session: &config::Session,
    task_id: Uuid,
    thread_id: &str,
    turn_id: &str,
    operation_id: Uuid,
    owner: &SessionInstance,
) -> Result<TaskRecord> {
    store.update_if_instance_live(session, task_id, owner, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        anyhow::ensure!(
            record.thread_id.as_deref() == Some(thread_id),
            "turn/start returned for an unexpected task thread"
        );
        if let Some(existing) = record.turn_id.as_deref() {
            anyhow::ensure!(
                existing == turn_id,
                "turn/start returned an unexpected turn id"
            );
        } else {
            record.turn_id = Some(turn_id.to_owned());
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
    })
}

fn apply_control_failure(
    store: &TaskStore,
    session: &config::Session,
    task_id: Uuid,
    operation_id: Uuid,
    shutting_down: bool,
) -> Result<TaskRecord> {
    store.update(session, task_id, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        record.status = if shutting_down {
            TaskStatus::Interrupted
        } else {
            TaskStatus::ReconciliationRequired
        };
        record.revision = record.revision.saturating_add(1);
        if shutting_down {
            update_operation_receipt(record, operation_id, OperationPhase::Applied);
        }
        Ok(())
    })
}

fn operation_view(task_id: Uuid, outcome: &OperationOutcome) -> Value {
    json!({
        "task_id": task_id,
        "status": outcome.status.as_str(),
        "revision": outcome.revision,
        "generation": outcome.generation,
        "thread_id": outcome.thread_id,
        "turn_id": outcome.turn_id,
        "reconciliation_required": outcome.status == TaskStatus::ReconciliationRequired,
    })
}

#[derive(Clone)]
struct RpcClient {
    tx: mpsc::Sender<ClientCommand>,
    actor: Arc<Mutex<Option<JoinHandle<()>>>>,
    connected: Arc<AtomicBool>,
    pending_approvals: Arc<AtomicU64>,
    pending_summary_binding: Arc<Mutex<Option<PendingSummaryBinding>>>,
    runtime_instance_id: Arc<Mutex<Option<Uuid>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingSummaryBinding {
    thread_id: String,
    turn_id: String,
    generation: u64,
}

impl RpcClient {
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    fn pending_summary_binding(&self) -> Option<PendingSummaryBinding> {
        self.pending_summary_binding.lock().unwrap().clone()
    }

    fn mark_pending_summary_ready(&self, record: &TaskRecord) {
        let binding = record
            .thread_id
            .as_ref()
            .zip(record.turn_id.as_ref())
            .filter(|_| record.generation > 0)
            .map(|(thread_id, turn_id)| PendingSummaryBinding {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                generation: record.generation,
            });
        *self.pending_summary_binding.lock().unwrap() = binding;
    }

    fn pending_summary_ready_for(&self, record: &TaskRecord) -> bool {
        let Some(thread_id) = record.thread_id.as_deref() else {
            return false;
        };
        let Some(turn_id) = record.turn_id.as_deref() else {
            return false;
        };
        record.generation > 0
            && self.pending_summary_binding().as_ref()
                == Some(&PendingSummaryBinding {
                    thread_id: thread_id.to_owned(),
                    turn_id: turn_id.to_owned(),
                    generation: record.generation,
                })
    }
}

enum ClientCommand {
    Request {
        method: &'static str,
        params: Value,
        reply: oneshot::Sender<std::result::Result<Value, String>>,
    },
    Notify {
        method: &'static str,
        params: Option<Value>,
    },
    Shutdown,
}

struct ServerResponse {
    id: Value,
    payload: std::result::Result<Value, (i64, String)>,
}

impl RpcClient {
    fn try_request(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<oneshot::Receiver<std::result::Result<Value, String>>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .try_send(ClientCommand::Request {
                method,
                params,
                reply: reply_tx,
            })
            .map_err(|error| {
                anyhow::anyhow!("Codex app-server actor queue unavailable: {error}")
            })?;
        Ok(reply_rx)
    }

    fn try_notify(&self, method: &'static str, params: Option<Value>) -> Result<()> {
        self.tx
            .try_send(ClientCommand::Notify { method, params })
            .map_err(|error| {
                anyhow::anyhow!("Codex app-server actor queue unavailable: {error}")
            })?;
        Ok(())
    }

    async fn request(&self, method: &'static str, params: Value) -> Result<Value> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ClientCommand::Request {
                method,
                params,
                reply: reply_tx,
            })
            .await
            .context("Codex app-server actor stopped")?;
        let result = tokio::time::timeout(RPC_TIMEOUT, reply_rx)
            .await
            .with_context(|| format!("Codex app-server request timed out: {method}"))?
            .context("Codex app-server actor dropped request")?;
        result.map_err(anyhow::Error::msg)
    }

    async fn notify(&self, method: &'static str, params: Option<Value>) -> Result<()> {
        self.tx
            .send(ClientCommand::Notify { method, params })
            .await
            .context("Codex app-server actor stopped")
    }

    async fn shutdown(&self) {
        let _ = self.shutdown_checked().await;
    }

    async fn shutdown_checked(&self) -> Result<()> {
        self.connected.store(false, Ordering::Release);
        let _ = self.tx.send(ClientCommand::Shutdown).await;
        let actor = self.actor.lock().unwrap().take();
        if let Some(actor) = actor {
            actor
                .await
                .context("Codex app-server shutdown did not complete cleanly")?;
        }
        Ok(())
    }
}

fn dispatch_request_for_instance(
    client: &RpcClient,
    owner: &SessionInstance,
    method: &'static str,
    params: Value,
) -> Result<oneshot::Receiver<std::result::Result<Value, String>>> {
    let registry = codex_lifecycle_registry().lock().unwrap();
    let entry = registry
        .entries
        .get(owner)
        .context("Codex session instance lifecycle state is unavailable")?;
    anyhow::ensure!(!entry.closing, "Codex session instance is closing");
    client.try_request(method, params)
}

fn dispatch_notify_for_instance(
    client: &RpcClient,
    owner: &SessionInstance,
    method: &'static str,
    params: Option<Value>,
) -> Result<()> {
    let registry = codex_lifecycle_registry().lock().unwrap();
    let entry = registry
        .entries
        .get(owner)
        .context("Codex session instance lifecycle state is unavailable")?;
    anyhow::ensure!(!entry.closing, "Codex session instance is closing");
    client.try_notify(method, params)
}

async fn wait_for_request_response(
    reply_rx: oneshot::Receiver<std::result::Result<Value, String>>,
    method: &'static str,
) -> Result<Value> {
    let result = tokio::time::timeout(RPC_TIMEOUT, reply_rx)
        .await
        .with_context(|| format!("Codex app-server request timed out: {method}"))?
        .context("Codex app-server actor dropped request")?;
    result.map_err(anyhow::Error::msg)
}

async fn request_for_instance(
    client: &RpcClient,
    owner: &SessionInstance,
    session: &config::Session,
    method: &'static str,
    params: Value,
) -> Result<Value> {
    let permit = ensure_current_active_instance(owner, session).await?;
    let reply_rx = dispatch_request_for_instance(client, owner, method, params)?;
    let mut cancellation = permit.cancellation.clone();
    tokio::select! {
        result = wait_for_request_response(reply_rx, method) => result,
        changed = cancellation.changed() => {
            let _ = changed;
            client.shutdown().await;
            Err(anyhow::anyhow!("Codex session instance began closing during {method}"))
        }
    }
}

async fn notify_for_instance(
    client: &RpcClient,
    owner: &SessionInstance,
    session: &config::Session,
    method: &'static str,
    params: Option<Value>,
) -> Result<()> {
    let permit = ensure_current_active_instance(owner, session).await?;
    dispatch_notify_for_instance(client, owner, method, params)?;
    drop(permit);
    Ok(())
}

async fn request_codex(
    client: &RpcClient,
    owner: &SessionInstance,
    session: &config::Session,
    method: &'static str,
    params: Value,
    fence: bool,
) -> Result<Value> {
    if fence {
        request_for_instance(client, owner, session, method, params).await
    } else {
        client.request(method, params).await
    }
}

async fn notify_codex(
    client: &RpcClient,
    owner: &SessionInstance,
    session: &config::Session,
    method: &'static str,
    params: Option<Value>,
    fence: bool,
) -> Result<()> {
    if fence {
        notify_for_instance(client, owner, session, method, params).await
    } else {
        client.notify(method, params).await
    }
}

#[derive(Clone)]
struct RuntimeHandle {
    client: RpcClient,
    owner: SessionInstance,
    scope: PathBuf,
    instance_id: Uuid,
    started_at: Instant,
    _lease: Arc<TaskRuntimeLease>,
}

fn runtimes() -> &'static Mutex<HashMap<Uuid, RuntimeHandle>> {
    static RUNTIMES: OnceLock<Mutex<HashMap<Uuid, RuntimeHandle>>> = OnceLock::new();
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime_for(session: &config::Session, task_id: Uuid) -> Option<RuntimeHandle> {
    let owner = SessionInstance::from_session(session);
    let lifecycle = codex_lifecycle_registry().lock().unwrap();
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

fn runtime_registration_matches(
    session: &config::Session,
    task_id: Uuid,
    runtime_instance_id: Uuid,
) -> bool {
    let owner = SessionInstance::from_session(session);
    if codex_lifecycle_registry()
        .lock()
        .unwrap()
        .entries
        .get(&owner)
        .is_some_and(|entry| entry.closing)
    {
        return false;
    }
    let Ok(scope) = config::canonical_directory(&session.cwd) else {
        return false;
    };
    runtimes()
        .lock()
        .unwrap()
        .get(&task_id)
        .is_some_and(|runtime| {
            runtime.instance_id == runtime_instance_id
                && runtime.owner == owner
                && runtime.scope == scope
                && Instant::now().saturating_duration_since(runtime.started_at) < CHILD_LIFETIME
                && runtime.client.is_connected()
        })
}

fn take_runtime_if_instance(task_id: Uuid, runtime_instance_id: Uuid) -> Option<RuntimeHandle> {
    let _guard = store_lock().lock().unwrap();
    let mut state = runtimes().lock().unwrap();
    if state
        .get(&task_id)
        .is_some_and(|runtime| runtime.instance_id == runtime_instance_id)
    {
        state.remove(&task_id)
    } else {
        None
    }
}

async fn insert_runtime(
    session: &config::Session,
    task_id: Uuid,
    store: &TaskStore,
    client: RpcClient,
    lease: Arc<TaskRuntimeLease>,
) -> Result<()> {
    let owner = SessionInstance::from_session(session);
    let _permit = ensure_current_active_instance(&owner, session).await?;
    insert_runtime_unchecked(session, task_id, store, client, &owner, lease)
}

async fn observe_pending_interactions(
    session: config::Session,
    task_id: Uuid,
    store: TaskStore,
    runtime_instance_id: Uuid,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(REFRESH_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if !observe_pending_interaction_step(
            &session,
            task_id,
            &store,
            runtime_instance_id,
            Duration::from_secs(REFRESH_INTERVAL_SECS),
        )
        .await
        {
            break;
        }
    }
}

async fn observe_pending_interaction_step(
    session: &config::Session,
    task_id: Uuid,
    store: &TaskStore,
    runtime_instance_id: Uuid,
    permit_timeout: Duration,
) -> bool {
    observe_pending_interaction_step_with(
        session,
        task_id,
        store,
        runtime_instance_id,
        permit_timeout,
        crate::pending_interaction::acquire_host_semaphore_permit(),
    )
    .await
}

async fn observe_pending_interaction_step_with<F>(
    session: &config::Session,
    task_id: Uuid,
    store: &TaskStore,
    runtime_instance_id: Uuid,
    permit_timeout: Duration,
    acquire_permit: F,
) -> bool
where
    F: std::future::Future<Output = Result<crate::pending_interaction::HostObservationPermit>>,
{
    if !runtime_registration_matches(session, task_id, runtime_instance_id) {
        return false;
    }
    let permit = match tokio::time::timeout(permit_timeout, acquire_permit).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) | Err(_) => return true,
    };
    if !runtime_registration_matches(session, task_id, runtime_instance_id) {
        drop(permit);
        return false;
    }
    let observed = store.observe_pending_interaction(session, task_id, runtime_instance_id, None);
    drop(permit);
    match observed {
        Ok(true) | Err(_) => true,
        Ok(false) => false,
    }
}

fn insert_runtime_unchecked(
    session: &config::Session,
    task_id: Uuid,
    store: &TaskStore,
    client: RpcClient,
    owner: &SessionInstance,
    lease: Arc<TaskRuntimeLease>,
) -> Result<()> {
    let owner = owner.clone();
    let store = store.clone();
    let session = session.clone();
    let scope = config::canonical_directory(&session.cwd)?;
    let runtime_instance_id = Uuid::new_v4();
    let runtime = RuntimeHandle {
        client: client.clone(),
        owner: owner.clone(),
        scope,
        instance_id: runtime_instance_id,
        started_at: Instant::now(),
        _lease: lease,
    };
    {
        let _guard = store_lock().lock().unwrap();
        let registry = codex_lifecycle_registry().lock().unwrap();
        anyhow::ensure!(
            registry
                .entries
                .get(&owner)
                .is_none_or(|entry| !entry.closing),
            "Codex session instance is closing"
        );
        let mut state = runtimes().lock().unwrap();
        anyhow::ensure!(
            !state.contains_key(&task_id),
            "Codex task runtime is already registered"
        );
        state.insert(task_id, runtime);
        *client.runtime_instance_id.lock().unwrap() = Some(runtime_instance_id);
    }
    let observer_session = session.clone();
    let observer_store = store.clone();
    tokio::spawn(async move {
        observe_pending_interactions(
            observer_session,
            task_id,
            observer_store,
            runtime_instance_id,
        )
        .await;
    });
    tokio::spawn(async move {
        let session_stopped = tokio::select! {
            _ = tokio::time::sleep(CHILD_LIFETIME) => false,
            _ = wait_for_session_stop(owner.clone()) => true,
        };
        let runtime = take_runtime_if_instance(task_id, runtime_instance_id);
        if let Some(runtime) = runtime {
            runtime.client.shutdown().await;
        }
        if session_stopped {
            loop {
                let result = async {
                    wait_for_session_inflight_drain(&owner, SESSION_CODEX_DRAIN_TIMEOUT).await?;
                    finalize_session_tasks(&owner, &store).await
                }
                .await;
                match result {
                    Ok(()) => break,
                    Err(error) => {
                        eprintln!(
                            "failed to finalize Codex tasks after session {} stopped; retrying: {error:#}",
                            owner.id
                        );
                        tokio::time::sleep(SESSION_STOP_POLL).await;
                    }
                }
            }
        }
    });
    Ok(())
}

async fn observe_session_instance(owner: &SessionInstance) -> SessionMonitorObservation {
    match config::read_session_metadata(&owner.id).await {
        Ok(session) if !owner.matches(&session) => SessionMonitorObservation::OwnerChanged,
        Ok(_) => match config::session_is_active(&owner.id).await {
            Ok(true) => SessionMonitorObservation::Active,
            Ok(false) => SessionMonitorObservation::Inactive,
            Err(_) => SessionMonitorObservation::Unknown(SessionMonitorUnknownKind::ActivityProbe),
        },
        Err(_) => SessionMonitorObservation::Unknown(SessionMonitorUnknownKind::MetadataRead),
    }
}

async fn wait_for_session_stop(owner: SessionInstance) {
    let mut monitor = SessionStopMonitor::default();
    loop {
        let observation = observe_session_instance(&owner).await;
        match monitor.observe(observation) {
            SessionMonitorAction::Continue => {
                if let SessionMonitorObservation::Unknown(kind) = observation
                    && monitor.consecutive_unknown == 1
                {
                    eprintln!(
                        "Codex session monitor observation is unknown; retrying accepted runtime \
                         (session {}, class {})",
                        owner.id,
                        kind.as_str()
                    );
                }
            }
            SessionMonitorAction::Stop(_) => {
                begin_session_instance_shutdown(&owner);
                return;
            }
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
    let deadline = Instant::now() + SESSION_CODEX_DRAIN_TIMEOUT;
    loop {
        let outcome = store
            .finalize_owner(owner)
            .context("failed to finalize Codex tasks for ended session instance")?;
        if !outcome.deferred {
            let _ = outcome.finalized;
            remove_session_evidence(owner).await;
            finish_session_shutdown(owner)?;
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out waiting for a remotely owned Codex task runtime to stop"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
async fn remove_session_with_store(session: &config::Session, store: &TaskStore) -> Result<()> {
    remove_session_with_store_with_timeout(session, store, SESSION_CODEX_DRAIN_TIMEOUT).await
}

#[cfg(test)]
async fn remove_session_with_store_with_timeout(
    session: &config::Session,
    store: &TaskStore,
    timeout: Duration,
) -> Result<()> {
    let owner = SessionInstance::from_session(session);
    begin_session_instance_shutdown(&owner);
    shutdown_session_runtimes(&owner).await;
    wait_for_session_inflight_drain(&owner, timeout).await?;
    finalize_session_tasks(&owner, store).await
}

pub(crate) async fn remove_session(session: &config::Session) -> Result<()> {
    let owner = SessionInstance::from_session(session);
    begin_session_instance_shutdown(&owner);
    shutdown_session_runtimes(&owner).await;
    wait_for_session_inflight_drain(&owner, SESSION_CODEX_DRAIN_TIMEOUT).await?;
    match TaskStore::default_store() {
        Ok(store) => finalize_session_tasks(&owner, &store).await,
        Err(error) => {
            remove_session_evidence(&owner).await;
            Err(error).context("cannot open Codex task store for session cleanup")
        }
    }
}

async fn spawn_initialized_client(
    session: &config::Session,
    task_id: Option<Uuid>,
) -> Result<(RpcClient, Value)> {
    let binary = configured_codex_binary()?;
    spawn_initialized_client_with_binary(session, task_id, &binary).await
}

/// Resolve an explicit host-owned executable before accepting work. The
/// compatibility name is supported by `environment::var_os`; task input never
/// selects an executable.
fn configured_codex_binary() -> Result<PathBuf> {
    let Some(configured) = temote_mcp::environment::var_os("TEMOTE_MCP_CODEX_BINARY") else {
        return Ok(PathBuf::from("codex"));
    };
    let path = PathBuf::from(configured);
    validate_configured_codex_binary(&path)
}

fn validate_configured_codex_binary(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "configured Codex binary must be absolute"
    );
    let resolved = path
        .canonicalize()
        .context("configured Codex binary is unavailable")?;
    let metadata = std::fs::metadata(&resolved)?;
    anyhow::ensure!(metadata.is_file(), "configured Codex binary is not a file");
    #[cfg(unix)]
    anyhow::ensure!(
        metadata.permissions().mode() & 0o111 != 0,
        "configured Codex binary is not executable"
    );
    // Validate the resolved target, but execute the configured invocation path.
    // Multicall executables select their applet from argv[0] (e.g. a `codex`
    // symlink to a host-owned launcher); canonicalizing it changes the applet.
    Ok(path.to_path_buf())
}

async fn spawn_initialized_client_with_binary(
    session: &config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
) -> Result<(RpcClient, Value)> {
    spawn_initialized_client_with_binary_mode(session, task_id, binary, true, None).await
}

async fn spawn_initialized_client_with_binary_mode(
    session: &config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
    fence: bool,
    runtime_lease: Option<Arc<TaskRuntimeLease>>,
) -> Result<(RpcClient, Value)> {
    let owner = SessionInstance::from_session(session);
    let spawn_permit = if fence {
        Some(ensure_current_active_instance(&owner, session).await?)
    } else {
        None
    };
    let client = if fence {
        spawn_client_if_instance_live(session.clone(), task_id, binary, &owner, runtime_lease)?
    } else {
        spawn_client_with_binary_unchecked(session.clone(), task_id, binary, runtime_lease)?
    };
    let initialized = async {
        let initialize_params = json!({
            "clientInfo": {
                "name": APP_SERVER_CLIENT_NAME,
                "version": APP_SERVER_CLIENT_VERSION
            },
            "capabilities": {
                "experimentalApi": true
            }
        });
        let initialized = request_codex(
            &client,
            &owner,
            session,
            "initialize",
            initialize_params,
            fence,
        )
        .await?;
        validate_initialize_response(&initialized)?;
        notify_codex(&client, &owner, session, "initialized", None, fence).await?;
        Result::<Value>::Ok(initialized)
    }
    .await;
    match initialized {
        Ok(initialized) => {
            drop(spawn_permit);
            Ok((client, initialized))
        }
        Err(error) => {
            client.shutdown().await;
            drop(spawn_permit);
            Err(error)
        }
    }
}

fn validate_initialize_response(value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .context("Codex app-server initialize result must be an object")?;
    let user_agent = object
        .get("userAgent")
        .and_then(Value::as_str)
        .context("Codex app-server initialize result is missing userAgent")?;
    anyhow::ensure!(
        !user_agent.is_empty() && user_agent.len() <= MAX_APP_SERVER_USER_AGENT_BYTES,
        "Codex app-server initialize result has invalid userAgent"
    );
    let codex_home = object
        .get("codexHome")
        .and_then(Value::as_str)
        .context("Codex app-server initialize result is missing codexHome")?;
    anyhow::ensure!(
        Path::new(codex_home).is_absolute(),
        "Codex app-server initialize result has invalid codexHome"
    );
    required_initialize_string(object, "platformFamily")?;
    required_initialize_string(object, "platformOs")?;
    Ok(())
}

fn app_server_version_from_initialize_response(value: &Value) -> Option<&str> {
    value
        .get("userAgent")
        .and_then(Value::as_str)
        .and_then(app_server_version_from_user_agent)
}

fn required_initialize_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("Codex app-server initialize result is missing {field}"))?;
    anyhow::ensure!(
        !value.is_empty(),
        "Codex app-server initialize result has invalid {field}"
    );
    Ok(value)
}

fn app_server_version_from_user_agent(user_agent: &str) -> Option<&str> {
    if user_agent.len() > MAX_APP_SERVER_USER_AGENT_BYTES {
        return None;
    }

    let product_and_metadata = user_agent.strip_prefix(APP_SERVER_CLIENT_NAME)?;
    let product_and_metadata = product_and_metadata.strip_prefix('/')?;
    let (version, metadata) = product_and_metadata.split_once(' ')?;

    let platform_and_rest = metadata.strip_prefix('(')?;
    let (platform, origin_and_client) = platform_and_rest.split_once(") ")?;
    let (platform_name, architecture) = platform.split_once("; ")?;
    if !bounded_user_agent_component(platform_name, 128, true)
        || !bounded_user_agent_component(architecture, 64, false)
    {
        return None;
    }

    let (origin, client) = origin_and_client.split_once(" (")?;
    if !bounded_user_agent_component(origin, 64, false) {
        return None;
    }
    let expected_client = format!("{APP_SERVER_CLIENT_NAME}; {APP_SERVER_CLIENT_VERSION})");
    (client == expected_client).then_some(version)
}

fn bounded_user_agent_component(value: &str, max_bytes: usize, allow_space: bool) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.chars().all(|character| {
            (character.is_ascii_graphic() || (allow_space && character == ' '))
                && !matches!(character, '(' | ')' | ';')
        })
}

fn spawn_client_if_instance_live(
    session: config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
    owner: &SessionInstance,
    runtime_lease: Option<Arc<TaskRuntimeLease>>,
) -> Result<RpcClient> {
    let registry = codex_lifecycle_registry().lock().unwrap();
    let entry = registry
        .entries
        .get(owner)
        .context("Codex session instance lifecycle state is unavailable")?;
    anyhow::ensure!(!entry.closing, "Codex session instance is closing");
    spawn_client_with_binary_unchecked(session, task_id, binary, runtime_lease)
}

fn spawn_client_with_binary_unchecked(
    session: config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
    runtime_lease: Option<Arc<TaskRuntimeLease>>,
) -> Result<RpcClient> {
    let mut command = Command::new(binary);
    command
        .arg("app-server")
        .arg("--stdio")
        .arg("-c")
        .arg("mcp_servers={}")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(&session.cwd)
        .kill_on_drop(true)
        .env_clear();
    for (key, value) in filtered_codex_environment(std::env::vars_os()) {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start Codex app-server from {}", binary.display()))?;
    let stdin = child
        .stdin
        .take()
        .context("Codex app-server stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Codex app-server stdout unavailable")?;
    let (tx, rx) = mpsc::channel(64);
    let connected = Arc::new(AtomicBool::new(true));
    let pending_approvals = Arc::new(AtomicU64::new(0));
    let runtime_instance_id = Arc::new(Mutex::new(None));
    let actor = tokio::spawn(run_actor(
        child,
        stdin,
        stdout,
        rx,
        RpcActorContext {
            session,
            task_id,
            runtime_lease_guard: runtime_lease,
            connected: Arc::clone(&connected),
            pending_approvals: Arc::clone(&pending_approvals),
            runtime_instance_id: Arc::clone(&runtime_instance_id),
        },
    ));
    Ok(RpcClient {
        tx,
        actor: Arc::new(Mutex::new(Some(actor))),
        connected,
        pending_approvals,
        pending_summary_binding: Arc::new(Mutex::new(None)),
        runtime_instance_id,
    })
}

fn filtered_codex_environment<I>(environment: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    environment
        .into_iter()
        .filter(|(key, _)| codex_environment_key_allowed(key))
        .collect()
}

fn codex_environment_key_allowed(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    key.starts_with("LC_") || CODEX_CHILD_ENV_ALLOWLIST.contains(&key)
}

struct RpcActorContext {
    session: config::Session,
    task_id: Option<Uuid>,
    runtime_lease_guard: Option<Arc<TaskRuntimeLease>>,
    connected: Arc<AtomicBool>,
    pending_approvals: Arc<AtomicU64>,
    runtime_instance_id: Arc<Mutex<Option<Uuid>>>,
}

struct ActorConnectionGuard(Arc<AtomicBool>);

impl Drop for ActorConnectionGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn run_actor(
    mut child: tokio::process::Child,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    mut commands: mpsc::Receiver<ClientCommand>,
    context: RpcActorContext,
) {
    let RpcActorContext {
        session,
        task_id,
        runtime_lease_guard,
        connected,
        pending_approvals,
        runtime_instance_id,
    } = context;
    let _connection_guard = ActorConnectionGuard(Arc::clone(&connected));
    let mut reader = BufReader::new(stdout);
    // Keep partial JSONL data outside the cancellable select future. If a
    // command arrives while fill_buf is waiting for the rest of a line, Tokio
    // drops only the read future; this buffer retains the bytes already
    // consumed from the child pipe for the next iteration.
    let mut partial_line = Vec::new();
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
                        let message = json!({"id": id, "method": method, "params": params});
                        if let Err(error) = write_json_line(&mut stdin, &message).await {
                            let _ = reply.send(Err(format!("Codex app-server write failed: {error:#}")));
                            break format!("Codex app-server write failed: {error:#}");
                        }
                        pending.insert(id, reply);
                    }
                    ClientCommand::Notify { method, params } => {
                        let message = match params {
                            Some(params) => json!({"method": method, "params": params}),
                            None => json!({"method": method}),
                        };
                        if let Err(error) = write_json_line(&mut stdin, &message).await {
                            break format!("Codex app-server notification write failed: {error:#}");
                        }
                    }
                    ClientCommand::Shutdown => break "shutdown requested".to_owned(),
                }
            }
            response = server_rx.recv() => {
                if let Some(response) = response {
                    let message = match response.payload {
                        Ok(result) => json!({"id": response.id, "result": result}),
                        Err((code, message)) => json!({"id": response.id, "error": {"code": code, "message": message}}),
                    };
                    if let Err(error) = write_json_line(&mut stdin, &message).await {
                        break format!("Codex app-server server-response write failed: {error:#}");
                    }
                }
            }
            line = read_bounded_json_line(&mut reader, &mut partial_line) => {
                match line {
                    Ok(Some(value)) => {
                        if let Some(method) = value.get("method").and_then(Value::as_str) {
                            let method = method.to_owned();
                            if let Some(id) = value.get("id").cloned() {
                                let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
                                let tx = server_tx.clone();
                                let session = session.clone();
                                let pending_approvals = Arc::clone(&pending_approvals);
                                let runtime_instance_id = Arc::clone(&runtime_instance_id);
                                tokio::spawn(async move {
                                    let payload = handle_server_request(
                                        &session,
                                        task_id,
                                        &method,
                                        params,
                                        pending_approvals,
                                        runtime_instance_id,
                                    ).await;
                                    let _ = tx.send(ServerResponse { id, payload }).await;
                                });
                            } else {
                                handle_notification(&session, task_id, &method, value.get("params"));
                            }
                        } else if let Some(id) = value.get("id").and_then(Value::as_u64)
                            && let Some(reply) = pending.remove(&id)
                        {
                            if let Some(error) = value.get("error") {
                                let _ = reply.send(Err(format!("Codex app-server RPC error: {error}")));
                            } else if let Some(result) = value.get("result") {
                                let _ = reply.send(Ok(result.clone()));
                            } else {
                                let _ = reply.send(Err("Codex app-server response has neither result nor error".to_owned()));
                            }
                        }
                    }
                    Ok(None) => break "Codex app-server stdout closed".to_owned(),
                    Err(error) => break format!("Codex app-server protocol error: {error:#}"),
                }
            }
            status = child.wait() => {
                break match status {
                    Ok(status) => format!("Codex app-server exited: {status}"),
                    Err(error) => format!("Codex app-server wait failed: {error}"),
                };
            }
        }
    };

    for (_, reply) in pending {
        let _ = reply.send(Err(terminal_error.clone()));
    }
    pending_approvals.store(0, Ordering::Release);
    let _ = child.kill().await;
    drop(runtime_lease_guard);
}

async fn write_json_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() <= MAX_RPC_LINE_BYTES,
        "outbound Codex app-server message exceeds {MAX_RPC_LINE_BYTES} bytes"
    );
    stdin.write_all(&bytes).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_bounded_json_line<R>(reader: &mut R, line: &mut Vec<u8>) -> Result<Option<Value>>
where
    R: AsyncBufRead + Unpin,
{
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
                "Codex app-server message exceeds {MAX_RPC_LINE_BYTES} bytes"
            );
            line.extend_from_slice(&chunk[..newline]);
            reader.consume(newline + 1);
            break;
        }
        anyhow::ensure!(
            line.len().saturating_add(chunk.len()) <= MAX_RPC_LINE_BYTES,
            "Codex app-server message exceeds {MAX_RPC_LINE_BYTES} bytes"
        );
        line.extend_from_slice(chunk);
        let len = chunk.len();
        reader.consume(len);
    }
    let value = serde_json::from_slice(line).context("invalid Codex app-server JSON line")?;
    line.clear();
    Ok(Some(value))
}

fn handle_notification(
    session: &config::Session,
    task_id: Option<Uuid>,
    method: &str,
    params: Option<&Value>,
) {
    let Some(task_id) = task_id else {
        return;
    };
    #[cfg(unix)]
    if let Some(candidate) = crate::codex_prompt_observer::parse_item_started(method, params) {
        let session = session.clone();
        tokio::spawn(async move {
            // Codex may emit item/started before the turn/start RPC response
            // has established the retained turn binding. Observe only after
            // that exact binding exists, with bounded best-effort retries.
            for attempt in 0..100 {
                if session_instance_is_closing(&SessionInstance::from_session(&session)) {
                    break;
                }
                let binding = (|| -> Result<Option<crate::codex_prompt_observer::Binding>> {
                    let store = TaskStore::default_store()?;
                    let _guard = store.lock()?;
                    let record = store.load_locked(&session, task_id)?;
                    Ok(prompt_notification_binding(&session, &record, &candidate))
                })();
                if let Ok(Some(binding)) = binding {
                    let _ = crate::codex_prompt_observer::observe(candidate, binding).await;
                    break;
                }
                if attempt < 99 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        });
        return;
    }
    let Some(params) = params else {
        return;
    };
    let Some(thread_id) = notification_thread_id(params) else {
        return;
    };
    let turn_id = notification_turn_id(params);
    let usage = notification_usage(params);
    let status = match method {
        "turn/started" => Some(TaskStatus::Running),
        "turn/completed" => params
            .get("turn")
            .and_then(|turn| turn.get("status"))
            .or_else(|| params.get("status"))
            .and_then(Value::as_str)
            .and_then(task_status_from_str),
        "thread/tokenUsage/updated" => None,
        _ => return,
    };
    let Ok(store) = TaskStore::default_store() else {
        return;
    };
    let owner = SessionInstance::from_session(session);
    let _ = store.update_if_instance_live(session, task_id, &owner, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        anyhow::ensure!(
            record.thread_id.as_deref() == Some(thread_id),
            "Codex notification does not match task thread"
        );
        // A predecessor notification cannot move a newer turn backwards.
        if record.previous_turn_id.as_deref() == turn_id.as_deref() {
            return Ok(());
        }
        if record.turn_id.is_some() && turn_id.as_deref() != record.turn_id.as_deref() {
            return Ok(());
        }
        let mut changed = false;
        if let Some(turn_id) = turn_id.as_deref()
            && record.turn_id.as_deref() != Some(turn_id)
        {
            record.turn_id = Some(turn_id.to_owned());
            record.previous_turn_id = None;
            record.generation = record.generation.max(1);
            changed = true;
        }
        if let Some(status) = status
            && record.status != status
            && (!record.status.is_terminal() || status.is_terminal())
        {
            record.status = status;
            changed = true;
        }
        if usage.is_some() && record.usage != usage {
            record.usage = usage.clone();
            changed = true;
        }
        if changed {
            record.revision = record.revision.saturating_add(1);
        }
        Ok(())
    });
}

#[cfg(unix)]
fn prompt_notification_binding(
    session: &config::Session,
    record: &TaskRecord,
    candidate: &crate::codex_prompt_observer::Candidate,
) -> Option<crate::codex_prompt_observer::Binding> {
    if !record.owner.matches(session)
        || record.scope_cwd != session.cwd
        || record.task_id.is_nil()
        || record.generation == 0
        || record.thread_id.as_deref() != Some(candidate.thread_id.as_str())
        || record.turn_id.as_deref() != Some(candidate.turn_id.as_str())
    {
        return None;
    }
    Some(crate::codex_prompt_observer::Binding {
        session: session.clone(),
        task_id: record.task_id.to_string(),
        execution_id: outcome::execution_id(record.task_id, record.generation).to_string(),
    })
}

fn notification_thread_id(params: &Value) -> Option<&str> {
    params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("threadId"))
                .and_then(Value::as_str)
        })
}

fn notification_turn_id(params: &Value) -> Option<String> {
    params
        .get("turnId")
        .or_else(|| params.get("turn_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn notification_usage(params: &Value) -> Option<BTreeMap<String, u64>> {
    params
        .get("usage")
        .or_else(|| params.get("tokenUsage"))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("usage")))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("tokenUsage")))
        .and_then(extract_usage)
}

fn extract_usage(value: &Value) -> Option<BTreeMap<String, u64>> {
    let object = value.as_object()?;
    let mut usage = BTreeMap::new();
    for (name, aliases) in [
        ("input_tokens", ["input_tokens", "inputTokens"]),
        (
            "cached_input_tokens",
            ["cached_input_tokens", "cachedInputTokens"],
        ),
        ("output_tokens", ["output_tokens", "outputTokens"]),
        (
            "reasoning_output_tokens",
            ["reasoning_output_tokens", "reasoningOutputTokens"],
        ),
        ("total_tokens", ["total_tokens", "totalTokens"]),
    ] {
        if let Some(value) = aliases
            .iter()
            .find_map(|alias| object.get(*alias).and_then(Value::as_u64))
        {
            usage.insert(name.to_owned(), value);
        }
    }
    (!usage.is_empty()).then_some(usage)
}

async fn handle_server_request(
    session: &config::Session,
    task_id: Option<Uuid>,
    method: &str,
    params: Value,
    pending_approvals: Arc<AtomicU64>,
    runtime_instance_id: Arc<Mutex<Option<Uuid>>>,
) -> std::result::Result<Value, (i64, String)> {
    match method {
        "item/commandExecution/requestApproval" => {
            increment_pending_approvals(&pending_approvals);
            let Some(task_id) = task_id else {
                decrement_pending_approvals(&pending_approvals);
                return Err((
                    -32601,
                    "approval request is unavailable outside a Codex task".to_owned(),
                ));
            };
            let owner = SessionInstance::from_session(session);
            if ensure_current_active_instance(&owner, session)
                .await
                .is_err()
            {
                decrement_pending_approvals(&pending_approvals);
                return Ok(json!({"decision": "decline"}));
            }
            let generation = match validate_approval_task(session, task_id, &params) {
                Ok(generation) => generation,
                Err(error) => {
                    decrement_pending_approvals(&pending_approvals);
                    publish_pending_interaction(session, task_id, None, &runtime_instance_id).await;
                    return Err(error);
                }
            };
            mark_waiting_approval(session, task_id, true);
            publish_pending_interaction(session, task_id, Some(generation), &runtime_instance_id)
                .await;
            let (detail, metadata) = child_approval(
                "command execution",
                "commandExecution",
                "command_execution",
                task_id,
                &params,
            );
            let allowed =
                request_child_approval(session, "Codex command approval", detail, metadata).await;
            mark_waiting_approval(session, task_id, false);
            decrement_pending_approvals(&pending_approvals);
            publish_pending_interaction(session, task_id, Some(generation), &runtime_instance_id)
                .await;
            Ok(json!({"decision": if allowed { "accept" } else { "decline" }}))
        }
        "item/fileChange/requestApproval" => {
            increment_pending_approvals(&pending_approvals);
            let Some(task_id) = task_id else {
                decrement_pending_approvals(&pending_approvals);
                return Err((
                    -32601,
                    "approval request is unavailable outside a Codex task".to_owned(),
                ));
            };
            let owner = SessionInstance::from_session(session);
            if ensure_current_active_instance(&owner, session)
                .await
                .is_err()
            {
                decrement_pending_approvals(&pending_approvals);
                return Ok(json!({"decision": "decline"}));
            }
            let generation = match validate_approval_task(session, task_id, &params) {
                Ok(generation) => generation,
                Err(error) => {
                    decrement_pending_approvals(&pending_approvals);
                    publish_pending_interaction(session, task_id, None, &runtime_instance_id).await;
                    return Err(error);
                }
            };
            mark_waiting_approval(session, task_id, true);
            publish_pending_interaction(session, task_id, Some(generation), &runtime_instance_id)
                .await;
            let (detail, metadata) =
                child_approval("file change", "fileChange", "file_change", task_id, &params);
            let allowed =
                request_child_approval(session, "Codex file-change approval", detail, metadata)
                    .await;
            mark_waiting_approval(session, task_id, false);
            decrement_pending_approvals(&pending_approvals);
            publish_pending_interaction(session, task_id, Some(generation), &runtime_instance_id)
                .await;
            Ok(json!({"decision": if allowed { "accept" } else { "decline" }}))
        }
        _ => Err((
            -32601,
            format!("unsupported Codex app-server request method: {method}"),
        )),
    }
}

async fn request_child_approval(
    session: &config::Session,
    operation: &str,
    detail: String,
    metadata: BTreeMap<String, String>,
) -> bool {
    let owner = SessionInstance::from_session(session);
    let permit = match ensure_current_active_instance(&owner, session).await {
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
                ensure_current_active_instance(&owner, session).await.is_ok()
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

fn child_approval(
    operation: &str,
    method: &str,
    operation_type: &str,
    task_id: Uuid,
    params: &Value,
) -> (String, BTreeMap<String, String>) {
    let thread_id = safe_approval_identifier(params, "threadId");
    let turn_id = safe_approval_identifier(params, "turnId");
    let item_id = safe_approval_identifier(params, "itemId");
    let mut metadata = BTreeMap::from([
        ("provenance".to_owned(), "codex_app_server".to_owned()),
        ("source".to_owned(), "codex_delegation".to_owned()),
        ("tool".to_owned(), format!("item/{method}/requestApproval")),
        ("operation_type".to_owned(), operation_type.to_owned()),
        ("target".to_owned(), format!("task:{task_id}")),
        ("task_id".to_owned(), task_id.to_string()),
        ("mutation".to_owned(), "true".to_owned()),
        ("read_only".to_owned(), "false".to_owned()),
        ("scope".to_owned(), "session_cwd".to_owned()),
        ("thread_id".to_owned(), thread_id.clone()),
        ("turn_id".to_owned(), turn_id.clone()),
        ("item_id".to_owned(), item_id.clone()),
    ]);
    let specifics = if operation_type == "command_execution" {
        let summary = command_approval_summary(params);
        metadata.insert("command".to_owned(), "argument_values_omitted".to_owned());
        metadata.insert("command_summary".to_owned(), summary.clone());
        format!("command: {summary}")
    } else {
        let count = params
            .get("changes")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        metadata.insert("change_count".to_owned(), count.to_string());
        metadata.insert("change_details".to_owned(), "omitted".to_owned());
        format!("file changes: {count} change(s); paths and patch details omitted")
    };
    (
        format!(
            "Codex delegated task approval\noperation: {operation}\nmutation: true\ntarget: task {task_id}\nscope: session working directory\nthread_id: {thread_id}\nturn_id: {turn_id}\nitem_id: {item_id}\n{specifics}"
        ),
        metadata,
    )
}

fn safe_approval_identifier(params: &Value, key: &str) -> String {
    let Some(value) = params.get(key).and_then(Value::as_str) else {
        return "(not provided)".to_owned();
    };
    let mut rendered = String::new();
    for character in value.chars() {
        let part = if character.is_control() {
            if character.is_ascii() {
                format!("\\x{:02x}", character as u32)
            } else {
                format!("\\u{{{:x}}}", character as u32)
            }
        } else {
            character.to_string()
        };
        if rendered.len().saturating_add(part.len()) > MAX_ARGUMENT_BYTES {
            rendered.push('…');
            break;
        }
        rendered.push_str(&part);
    }
    rendered
}

fn command_approval_summary(params: &Value) -> String {
    let Some(command) = params.get("command").and_then(Value::as_array) else {
        return "provided; argument values omitted".to_owned();
    };
    let executable = command
        .first()
        .and_then(Value::as_str)
        .map(safe_command_component)
        .unwrap_or_else(|| "(not provided)".to_owned());
    let mut options = command
        .iter()
        .skip(1)
        .filter_map(Value::as_str)
        .filter(|value| value.starts_with('-'))
        .map(|value| {
            let name = value
                .split_once('=')
                .map_or(value, |(name, _)| name)
                .to_owned();
            safe_command_component(&name)
        })
        .collect::<Vec<_>>();
    options.sort();
    options.dedup();
    format!(
        "executable {executable}; {} argument(s); option names: {}",
        command.len().saturating_sub(1),
        if options.is_empty() {
            "(none)".to_owned()
        } else {
            options.join(", ")
        }
    )
}

fn safe_command_component(value: &str) -> String {
    let mut rendered = String::new();
    for character in value.chars() {
        if character.is_control() || character.is_whitespace() {
            break;
        }
        if rendered.len() >= MAX_ARGUMENT_BYTES {
            rendered.push('…');
            break;
        }
        rendered.push(character);
    }
    if rendered.is_empty() {
        "(not provided)".to_owned()
    } else {
        rendered
    }
}

fn validate_approval_task(
    session: &config::Session,
    task_id: Uuid,
    params: &Value,
) -> std::result::Result<u64, (i64, String)> {
    let store = TaskStore::default_store().map_err(internal_server_error)?;
    bind_approval_turn(&store, session, task_id, params)
        .map_err(|error| (-32602, error.to_string()))
}

fn bind_approval_turn(
    store: &TaskStore,
    session: &config::Session,
    task_id: Uuid,
    params: &Value,
) -> Result<u64> {
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .context("approval request is missing threadId")?;
    let turn_id = params
        .get("turnId")
        .and_then(Value::as_str)
        .context("approval request is missing turnId")?;
    let record = store.update(session, task_id, |record| {
        anyhow::ensure!(
            !record.status.is_terminal(),
            "approval request arrived after the Codex task was finalized"
        );
        anyhow::ensure!(
            record.thread_id.as_deref() == Some(thread_id),
            "approval request does not match task thread"
        );
        match record.turn_id.as_deref() {
            Some(existing) => anyhow::ensure!(
                existing == turn_id,
                "approval request does not match task turn"
            ),
            None => {
                record.turn_id = Some(turn_id.to_owned());
                record.revision = record.revision.saturating_add(1);
            }
        }
        Ok(())
    })?;
    Ok(record.generation)
}

fn internal_server_error(error: anyhow::Error) -> (i64, String) {
    (-32603, error.to_string())
}

fn mark_waiting_approval(session: &config::Session, task_id: Uuid, waiting: bool) {
    if let Ok(store) = TaskStore::default_store() {
        let _ = store.update(session, task_id, |record| {
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
}

fn increment_pending_approvals(pending_approvals: &AtomicU64) {
    let _ = pending_approvals.fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
        Some(pending.saturating_add(1))
    });
}

fn decrement_pending_approvals(pending_approvals: &AtomicU64) {
    let _ = pending_approvals.fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
        Some(pending.saturating_sub(1))
    });
}

async fn publish_pending_interaction(
    session: &config::Session,
    task_id: Uuid,
    expected_generation: Option<u64>,
    runtime_instance_id: &Mutex<Option<Uuid>>,
) {
    let Some(runtime_instance_id) = *runtime_instance_id.lock().unwrap() else {
        return;
    };
    let Ok(Ok(_permit)) = tokio::time::timeout(
        Duration::from_secs(REFRESH_INTERVAL_SECS),
        crate::pending_interaction::acquire_host_semaphore_permit(),
    )
    .await
    else {
        return;
    };
    if !runtime_registration_matches(session, task_id, runtime_instance_id) {
        return;
    }
    let Ok(store) = TaskStore::default_store() else {
        return;
    };
    let _ = store.observe_pending_interaction(
        session,
        task_id,
        runtime_instance_id,
        expected_generation,
    );
}

fn advertised_effort_name(entry: &Value) -> Option<&str> {
    // Observed Codex app-server schemas advertise `ReasoningEffortOption` objects with
    // a `reasoningEffort` field. Accept the legacy `effort` key and a bare string so a
    // schema alias never silently empties the advertised effort set.
    entry
        .get("reasoningEffort")
        .and_then(Value::as_str)
        .or_else(|| entry.get("effort").and_then(Value::as_str))
        .or_else(|| entry.as_str())
}

fn advertised_model(entry: &Value) -> Option<Value> {
    let model = entry.get("model")?.as_str()?;
    let efforts = entry
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| advertised_effort_name(item).map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut advertised = json!({"model": model, "efforts": efforts});
    for (native, public) in [("hidden", "hidden"), ("isDefault", "is_default")] {
        if let Some(value) = entry.get(native).and_then(Value::as_bool) {
            advertised[public] = json!(value);
        }
    }
    if let Some(effort) = entry.get("defaultReasoningEffort").and_then(Value::as_str) {
        advertised["default_effort"] = json!(effort);
    }
    Some(advertised)
}

fn validate_model_request(models: &Value, model: &str, effort: &str) -> Result<()> {
    let data = models
        .get("data")
        .and_then(Value::as_array)
        .context("Codex model/list response is missing data")?;
    let selected = data
        .iter()
        .find(|entry| entry.get("model").and_then(Value::as_str) == Some(model))
        .with_context(|| format!("Codex model is not advertised: {model}"))?;
    let efforts = selected
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .context("Codex model is missing supportedReasoningEfforts")?;
    anyhow::ensure!(
        efforts
            .iter()
            .any(|entry| advertised_effort_name(entry) == Some(effort)),
        "Codex effort {effort} is not advertised for model {model}"
    );
    Ok(())
}

pub(crate) async fn status(session: &config::Session) -> Result<Value> {
    let owner = SessionInstance::from_session(session);
    let (client, initialized) = spawn_initialized_client(session, None).await?;
    let models = match request_for_instance(
        &client,
        &owner,
        session,
        "model/list",
        json!({"includeHidden": true}),
    )
    .await
    {
        Ok(models) => models,
        Err(error) => {
            client.shutdown().await;
            return Err(error);
        }
    };
    client.shutdown().await;
    let data = models
        .get("data")
        .and_then(Value::as_array)
        .context("Codex model/list response is missing data")?;
    let advertised = data.iter().filter_map(advertised_model).collect::<Vec<_>>();
    let app_server_version = app_server_version_from_initialize_response(&initialized);
    Ok(json!({
        "compatible": true,
        "app_server_version": app_server_version,
        "platform_family": initialized.get("platformFamily"),
        "platform_os": initialized.get("platformOs"),
        "models": advertised,
    }))
}

pub(crate) async fn task_start(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    let binary = configured_codex_binary()?;
    task_start_with_store_and_binary_inner(
        args,
        session,
        &store,
        &binary,
        true,
        &TaskStartOrigin::Generic,
        || Ok(()),
    )
    .await
}

/// Return the retained result for an exact task-start retry without applying
/// any new filesystem precondition. A caller uses this before checking a
/// destination that the already-accepted task may itself have created.
pub(crate) fn task_start_replay_if_retained(
    args: &Value,
    session: &config::Session,
    origin: &TaskStartOrigin,
) -> Result<Option<Value>> {
    let store = TaskStore::default_store()?;
    task_start_replay_if_retained_with_store(args, session, origin, &store)
}

/// Read an exact retained start receipt even when the pre-thread start is
/// retryable. Private status uses this to inspect an already admitted check;
/// it never dispatches a new task.
pub(crate) fn task_start_receipt_if_retained(
    args: &Value,
    session: &config::Session,
    origin: &TaskStartOrigin,
) -> Result<Option<Value>> {
    let store = TaskStore::default_store()?;
    let operation_id = required_uuid(args, "operation_id")?;
    let task = required_string(args, "task")?;
    let model = required_string(args, "model")?;
    let effort = required_string(args, "effort")?;
    validate_task_input(task, "task")?;
    validate_argument(model, "model")?;
    validate_argument(effort, "effort")?;
    let continuation = codex_continuation(args)?;
    let task_id = task_id_for_operation(session, operation_id)?;
    let request_fingerprint =
        continuation_fingerprint(task_id, task, model, effort, origin, continuation)?;
    match store.read_record(task_id) {
        Ok(record) => {
            ensure_task_owner(&record, session)?;
            replay_operation(&record, operation_id, request_fingerprint).map(Some)
        }
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn task_start_replay_if_retained_with_store(
    args: &Value,
    session: &config::Session,
    origin: &TaskStartOrigin,
    store: &TaskStore,
) -> Result<Option<Value>> {
    let operation_id = required_uuid(args, "operation_id")?;
    let task = required_string(args, "task")?;
    let model = required_string(args, "model")?;
    let effort = required_string(args, "effort")?;
    validate_task_input(task, "task")?;
    validate_argument(model, "model")?;
    validate_argument(effort, "effort")?;
    let continuation = codex_continuation(args)?;
    let task_id = task_id_for_operation(session, operation_id)?;
    let request_fingerprint =
        continuation_fingerprint(task_id, task, model, effort, origin, continuation)?;
    match store.read_record(task_id) {
        Ok(record) => {
            ensure_task_owner(&record, session)?;
            let retryable_before_thread = record
                .operations
                .iter()
                .find(|receipt| receipt.operation_id == operation_id)
                .is_some_and(|receipt| {
                    receipt.request_fingerprint == request_fingerprint
                        && receipt.action == "start"
                        && receipt.phase == OperationPhase::RetryableFailed
                        && record.status == TaskStatus::RetryableFailed
                        && record.thread_id.is_none()
                });
            if retryable_before_thread {
                return Ok(None);
            }
            replay_operation(&record, operation_id, request_fingerprint).map(Some)
        }
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Seed an exact completed start receipt in the process-private default store.
/// This exercises production fingerprinting and replay without starting a
/// provider; it is intentionally unavailable outside unit tests.
#[cfg(test)]
pub(crate) fn seed_completed_task_for_test(
    args: &Value,
    session: &config::Session,
    origin: &TaskStartOrigin,
) -> Result<Uuid> {
    let store = TaskStore::default_store()?;
    let operation_id = required_uuid(args, "operation_id")?;
    let task = required_string(args, "task")?;
    let model = required_string(args, "model")?;
    let effort = required_string(args, "effort")?;
    validate_task_input(task, "task")?;
    validate_argument(model, "model")?;
    validate_argument(effort, "effort")?;
    let task_id = task_id_for_operation(session, operation_id)?;
    let request_fingerprint = task_start_fingerprint(task_id, task, model, effort, origin)?;
    let now = config::unix_time();
    let mut record = TaskRecord {
        schema_version: TASK_SCHEMA_VERSION,
        task_id,
        owner: SessionInstance::from_session(session),
        scope_cwd: config::canonical_directory(&session.cwd)?,
        model: model.to_owned(),
        effort: effort.to_owned(),
        status: TaskStatus::Completed,
        revision: 2,
        generation: 1,
        thread_id: Some("test-completed-thread".to_owned()),
        continued_from_task_id: None,
        continued_by_task_id: None,
        continued_by_request_fingerprint: None,
        turn_id: Some("test-completed-turn".to_owned()),
        previous_turn_id: None,
        usage: None,
        report: None,
        report_source: None,
        report_status: None,
        pending_interaction: None,
        created_at: now,
        updated_at: now,
        verification: None,
        delivery: None,
        operations: Vec::new(),
        operation_tombstones: Vec::new(),
    };
    record.operations.push(OperationReceipt {
        operation_id,
        request_fingerprint,
        action: "start".to_owned(),
        phase: OperationPhase::Applied,
        outcome: record.outcome(),
    });
    store.save(&record)?;
    Ok(task_id)
}

/// Start a Codex task after its durable acceptance while rechecking a
/// caller-supplied filesystem admission immediately before child startup.
pub(crate) async fn task_start_with_admission<F>(
    args: &Value,
    session: &config::Session,
    origin: &TaskStartOrigin,
    admission: F,
) -> Result<Value>
where
    F: FnOnce() -> Result<()>,
{
    let store = TaskStore::default_store()?;
    let binary = configured_codex_binary()?;
    task_start_with_store_and_binary_inner(args, session, &store, &binary, true, origin, admission)
        .await
}

#[cfg(test)]
async fn task_start_with_store_and_binary(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
) -> Result<Value> {
    task_start_with_store_and_binary_inner(
        args,
        session,
        store,
        binary,
        false,
        &TaskStartOrigin::Generic,
        || Ok(()),
    )
    .await
}

#[cfg(test)]
async fn task_start_with_store_and_binary_fenced(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
) -> Result<Value> {
    task_start_with_store_and_binary_inner(
        args,
        session,
        store,
        binary,
        true,
        &TaskStartOrigin::Generic,
        || Ok(()),
    )
    .await
}

#[cfg(test)]
async fn task_start_with_store_binary_and_admission<F>(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
    origin: &TaskStartOrigin,
    admission: F,
) -> Result<Value>
where
    F: FnOnce() -> Result<()>,
{
    task_start_with_store_and_binary_inner(args, session, store, binary, false, origin, admission)
        .await
}

async fn task_start_with_store_and_binary_inner<F>(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
    fence: bool,
    origin: &TaskStartOrigin,
    admission: F,
) -> Result<Value>
where
    F: FnOnce() -> Result<()>,
{
    let owner = SessionInstance::from_session(session);
    let operation_id = required_uuid(args, "operation_id")?;
    let task = required_string(args, "task")?;
    let model = required_string(args, "model")?;
    let effort = required_string(args, "effort")?;
    validate_task_input(task, "task")?;
    validate_argument(model, "model")?;
    validate_argument(effort, "effort")?;

    let task_id = task_id_for_operation(session, operation_id)?;
    let continuation = codex_continuation(args)?;
    let request_fingerprint =
        continuation_fingerprint(task_id, task, model, effort, origin, continuation)?;
    let now = config::unix_time();
    let mut record = TaskRecord {
        schema_version: TASK_SCHEMA_VERSION,
        task_id,
        owner: SessionInstance::from_session(session),
        scope_cwd: config::canonical_directory(&session.cwd)?,
        model: model.to_owned(),
        effort: effort.to_owned(),
        status: TaskStatus::Accepted,
        revision: 1,
        generation: 0,
        thread_id: None,
        continued_from_task_id: match continuation {
            CodexContinuation::New => None,
            CodexContinuation::PreviousTask { task_id } => Some(task_id),
        },
        continued_by_task_id: None,
        continued_by_request_fingerprint: None,
        turn_id: None,
        previous_turn_id: None,
        usage: None,
        report: None,
        report_source: None,
        report_status: None,
        pending_interaction: None,
        created_at: now,
        updated_at: now,
        verification: None,
        delivery: None,
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
    let acceptance_permit = if fence {
        Some(ensure_current_active_instance(&owner, session).await?)
    } else {
        None
    };
    let acceptance = if fence {
        store.accept_start_if_instance_live(session, record, &owner)?
    } else {
        store.accept_start(session, record)?
    };
    drop(acceptance_permit);
    let (mut record, runtime_lease, retired_runtime, resume_thread) = match acceptance {
        StartAcceptance::Existing(existing) => {
            return replay_operation(&existing, operation_id, request_fingerprint);
        }
        StartAcceptance::Accepted(record, lease, runtime, thread) => {
            (record, lease, runtime, thread)
        }
    };
    if let Some(runtime) = retired_runtime {
        let source_lease = Arc::clone(&runtime._lease);
        let shutdown = runtime.client.shutdown_checked().await;
        drop(runtime);
        drop(source_lease);
        if let Err(error) = shutdown {
            store.update(session, task_id, |record| {
                record.status = TaskStatus::Failed;
                record.revision = record.revision.saturating_add(1);
                update_operation_receipt(record, operation_id, OperationPhase::Applied);
                Ok(())
            })?;
            return Err(error)
                .context("CODEX_CONTINUATION_HANDOFF_FAILED: source runtime could not be retired");
        }
    }
    if let Err(error) = admission() {
        drop(runtime_lease);
        store.update(session, task_id, |record| {
            record.status = TaskStatus::Failed;
            record.revision = record.revision.saturating_add(1);
            update_operation_receipt(record, operation_id, OperationPhase::Applied);
            Ok(())
        })?;
        return Err(error)
            .context("repository clone filesystem admission changed before delegated side effect");
    }
    let runtime_lease = Arc::new(runtime_lease);
    let _operation_permit = if fence {
        Some(ensure_current_active_instance(&owner, session).await?)
    } else {
        None
    };

    let client_result = spawn_initialized_client_with_binary_mode(
        session,
        Some(task_id),
        binary,
        fence,
        Some(Arc::clone(&runtime_lease)),
    )
    .await;
    let (client, _) = match client_result {
        Ok(client) => client,
        Err(_) => {
            let shutting_down = fence && session_instance_is_closing(&owner);
            let record = store.update(session, task_id, |record| {
                if !record.status.is_terminal() {
                    record.status = if shutting_down {
                        TaskStatus::Interrupted
                    } else {
                        TaskStatus::RetryableFailed
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
            })?;
            return Ok(task_view(&record, None));
        }
    };
    let models = match request_codex(
        &client,
        &owner,
        session,
        "model/list",
        json!({"includeHidden": true}),
        fence,
    )
    .await
    {
        Ok(models) => models,
        Err(_) => {
            client.shutdown().await;
            let shutting_down = fence && session_instance_is_closing(&owner);
            let record = store.update(session, task_id, |record| {
                if !record.status.is_terminal() {
                    record.status = if shutting_down {
                        TaskStatus::Interrupted
                    } else {
                        TaskStatus::RetryableFailed
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
            })?;
            return Ok(task_view(&record, None));
        }
    };
    if validate_model_request(&models, model, effort).is_err() {
        client.shutdown().await;
        let shutting_down = fence && session_instance_is_closing(&owner);
        let record = store.update(session, task_id, |record| {
            if !record.status.is_terminal() {
                record.status = if shutting_down {
                    TaskStatus::Interrupted
                } else {
                    TaskStatus::Failed
                };
                record.revision = record.revision.saturating_add(1);
                update_operation_receipt(record, operation_id, OperationPhase::Applied);
            }
            Ok(())
        })?;
        return Ok(task_view(&record, None));
    }

    let start_result = async {
        let cwd = record.scope_cwd.to_string_lossy().into_owned();
        let (thread_method, thread_params) = if let Some(source_thread) = &resume_thread {
            (
                "thread/resume",
                json!({
                    "threadId": source_thread,
                    "cwd": cwd,
                    "model": model,
                    "approvalPolicy": CODEX_APPROVAL_POLICY,
                    "approvalsReviewer": "user",
                    "sandbox": "workspace-write",
                    "runtimeWorkspaceRoots": [record.scope_cwd],
                    "excludeTurns": true,
                }),
            )
        } else {
            (
                "thread/start",
                json!({
                    "cwd": cwd,
                    "model": model,
                    "approvalPolicy": CODEX_APPROVAL_POLICY,
                    "approvalsReviewer": "user",
                    "sandbox": "workspace-write",
                    "runtimeWorkspaceRoots": [record.scope_cwd],
                    "ephemeral": false,
                    "threadSource": "temote-mcp",
                }),
            )
        };
        let thread = request_codex(
            &client,
            &owner,
            session,
            thread_method,
            thread_params,
            fence,
        )
        .await?;
        let thread_id = thread
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .with_context(|| format!("{thread_method} response is missing thread.id"))?
            .to_owned();
        anyhow::ensure!(
            resume_thread
                .as_ref()
                .is_none_or(|source| source == &thread_id),
            "CODEX_CONTINUATION_THREAD_MISMATCH: resumed a different conversation"
        );
        let thread_update_permit = if fence {
            Some(ensure_current_active_instance(&owner, session).await?)
        } else {
            None
        };
        record = if fence {
            apply_thread_start_response_for_instance(store, session, task_id, &thread_id, &owner)?
        } else {
            apply_thread_start_response(store, session, task_id, &thread_id)?
        };
        drop(thread_update_permit);

        let turn = request_codex(
            &client,
            &owner,
            session,
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": task}],
                "model": model,
                "effort": effort,
                "outputSchema": crate::report_contract::codex_report_json_schema(),
                "cwd": cwd,
                "approvalPolicy": CODEX_APPROVAL_POLICY,
                "sandboxPolicy": {
                    "type": "workspaceWrite",
                    "writableRoots": [record.scope_cwd],
                    "networkAccess": false,
                },
            }),
            fence,
        )
        .await?;
        let turn_id = turn
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .context("turn/start response is missing turn.id")?
            .to_owned();
        let turn_update_permit = if fence {
            Some(ensure_current_active_instance(&owner, session).await?)
        } else {
            None
        };
        record = if fence {
            apply_turn_start_response_for_instance(
                store,
                session,
                task_id,
                &thread_id,
                &turn_id,
                operation_id,
                &owner,
            )?
        } else {
            apply_turn_start_response(store, session, task_id, &thread_id, &turn_id, operation_id)?
        };
        drop(turn_update_permit);
        Result::<()>::Ok(())
    }
    .await;

    if let Err(error) = start_result {
        client.shutdown().await;
        let shutting_down = fence && session_instance_is_closing(&owner);
        let record = store.update(session, task_id, |record| {
            if !record.status.is_terminal() {
                record.status = if shutting_down {
                    TaskStatus::Interrupted
                } else {
                    TaskStatus::ReconciliationRequired
                };
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
        if resume_thread.is_some() {
            return Err(error).context("CODEX_CONTINUATION_UNAVAILABLE: source conversation could not be resumed or advanced; reconcile the retained successor task");
        }
        return Ok(task_view(&record, None));
    }

    // A completed, admitted `turn/start` binds the new in-process runtime to
    // this exact thread, turn, and task generation. Reconnected clients remain
    // unready until a scoped `thread/read` validates the retained state.
    client.mark_pending_summary_ready(&record);

    let insert_result = if fence {
        insert_runtime(session, task_id, store, client.clone(), runtime_lease).await
    } else {
        insert_runtime_unchecked(
            session,
            task_id,
            store,
            client.clone(),
            &owner,
            runtime_lease,
        )
    };
    if let Err(error) = insert_result {
        client.shutdown().await;
        return Err(error);
    }
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

struct ReconciledRecord {
    record: TaskRecord,
    deferred: bool,
}

fn reconcile_snapshot<F>(
    session: &config::Session,
    owner: &SessionInstance,
    store: &TaskStore,
    expected: &TaskRecord,
    apply: F,
) -> Result<ReconciledRecord>
where
    F: FnOnce(&mut TaskRecord) -> Result<()>,
{
    let mut deferred = false;
    let record = store.update_if_instance_live(session, expected.task_id, owner, |record| {
        if record.revision != expected.revision
            || record.generation != expected.generation
            || record.thread_id != expected.thread_id
            || record.turn_id != expected.turn_id
        {
            deferred = true;
            return Ok(());
        }
        apply(record)
    })?;
    Ok(ReconciledRecord { record, deferred })
}

pub(crate) async fn task_get(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    let binary = configured_codex_binary()?;
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
    if session_instance_is_closing(&owner) {
        if let Ok(current) = config::read_session_metadata(&owner.id).await {
            anyhow::ensure!(
                owner.matches(&current),
                "Codex session instance is no longer current"
            );
        }
        let record = {
            let _guard = store.lock()?;
            store.load_locked(session, task_id)?
        };
        let mut view = task_view(&record, None);
        view["recovery_state"] = json!("owner_closing");
        return Ok(view);
    }
    let load_permit = ensure_current_active_instance(&owner, session).await?;
    let (mut record, runtime_access) = store.load_for_reconciliation(session, task_id)?;
    drop(load_permit);
    if record.continued_by_task_id.is_some() {
        // The old task stays readable, but its thread is now owned by the
        // successor. In particular, do not reconstruct a source runtime.
        return Ok(task_view_at_revision(&record, after_revision));
    }
    if task_control_in_flight(&record) {
        return Ok(task_view_at_revision(&record, after_revision));
    }
    let acquired_lease = match runtime_access {
        RuntimeAccess::Local => None,
        RuntimeAccess::Acquired(lease) => Some(lease),
        RuntimeAccess::OwnedElsewhere => {
            return Ok(task_view_at_revision(&record, after_revision));
        }
    };
    if record.thread_id.is_none() {
        if record.status == TaskStatus::Accepted {
            let start_operation_id = record
                .operations
                .iter()
                .find(|receipt| receipt.action == "start")
                .map(|receipt| receipt.operation_id);
            let apply_permit = ensure_current_active_instance(&owner, session).await?;
            let reconciled = reconcile_snapshot(session, &owner, store, &record, |record| {
                if record.status != TaskStatus::Accepted {
                    return Ok(());
                }
                if record.status != TaskStatus::ReconciliationRequired {
                    record.status = TaskStatus::ReconciliationRequired;
                    record.revision = record.revision.saturating_add(1);
                }
                if let Some(operation_id) = start_operation_id {
                    update_operation_receipt(record, operation_id, OperationPhase::Accepted);
                }
                Ok(())
            })?;
            drop(apply_permit);
            if reconciled.deferred {
                return Ok(task_view_at_revision(&reconciled.record, after_revision));
            }
            record = reconciled.record;
        }
        return Ok(reconciled_task_view(&record, None, after_revision));
    }

    let client =
        match ensure_runtime_with_binary(session, &record, store, binary, acquired_lease).await {
            Ok(EnsuredRuntime::Local(client)) => client,
            Ok(EnsuredRuntime::OwnedElsewhere) => {
                return Ok(task_view_at_revision(&record, after_revision));
            }
            Err(_) => {
                let apply_permit = ensure_current_active_instance(&owner, session).await?;
                let reconciled = reconcile_snapshot(session, &owner, store, &record, |record| {
                    if !record.status.is_terminal() && record.status != TaskStatus::Unknown {
                        record.status = TaskStatus::Unknown;
                        record.revision = record.revision.saturating_add(1);
                    }
                    Ok(())
                })?;
                drop(apply_permit);
                return Ok(if reconciled.deferred {
                    task_view_at_revision(&reconciled.record, after_revision)
                } else {
                    reconciled_task_view(&reconciled.record, None, after_revision)
                });
            }
        };
    let thread_id = record.thread_id.clone().unwrap();
    let response = request_for_instance(
        &client,
        &owner,
        session,
        "thread/read",
        json!({"threadId": thread_id, "includeTurns": true}),
    )
    .await;
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            let apply_permit = ensure_current_active_instance(&owner, session).await?;
            let reconciled = reconcile_snapshot(session, &owner, store, &record, |record| {
                if !record.status.is_terminal() && record.status != TaskStatus::Unknown {
                    record.status = TaskStatus::Unknown;
                    record.revision = record.revision.saturating_add(1);
                }
                Ok(())
            })?;
            drop(apply_permit);
            return Ok(if reconciled.deferred {
                task_view_at_revision(&reconciled.record, after_revision)
            } else {
                reconciled_task_view(&reconciled.record, None, after_revision)
            });
        }
    };
    let derived = match derive_thread_state_after(
        &response,
        record.turn_id.as_deref(),
        record.previous_turn_id.as_deref(),
    ) {
        Ok(derived) => derived,
        Err(_) => {
            let apply_permit = ensure_current_active_instance(&owner, session).await?;
            let reconciled = reconcile_snapshot(session, &owner, store, &record, |record| {
                if !record.status.is_terminal() && record.status != TaskStatus::Unknown {
                    record.status = TaskStatus::Unknown;
                    record.revision = record.revision.saturating_add(1);
                }
                Ok(())
            })?;
            drop(apply_permit);
            if reconciled.deferred {
                return Ok(task_view_at_revision(&reconciled.record, after_revision));
            }
            record = reconciled.record;
            if after_revision == Some(record.revision) {
                return Ok(reconciled_task_view(&record, None, after_revision));
            }
            let evidence_permit = ensure_current_active_instance(&owner, session).await?;
            let evidence_ref = store_evidence_for_instance(&owner, session, &response);
            drop(evidence_permit);
            return Ok(reconciled_task_view(
                &record,
                evidence_ref.as_ref(),
                after_revision,
            ));
        }
    };
    let apply_permit = ensure_current_active_instance(&owner, session).await?;
    let reconciled = reconcile_snapshot(session, &owner, store, &record, |record| {
        if record.status.is_terminal() {
            let previous = record.clone();
            if let Some(usage) = &derived.usage {
                record.usage = Some(usage.clone());
            }
            if derived.report_status.is_some()
                && (record.report != derived.report
                    || record.report_status != derived.report_status
                    || record.report_source.as_deref() != Some("native_structured_output"))
            {
                record.report = derived.report.clone();
                record.report_status = derived.report_status.clone();
                record.report_source = Some("native_structured_output".to_owned());
            }
            if record.usage != previous.usage
                || record.report != previous.report
                || record.report_status != previous.report_status
                || record.report_source != previous.report_source
            {
                record.revision = record.revision.saturating_add(1);
            }
            return Ok(());
        }
        let previous = record.clone();
        record.status = reconciled_task_status(record.status, derived.status);
        if derived.turn_id.is_some() {
            record.turn_id = derived.turn_id.clone();
            if record.generation == 0 {
                record.generation = 1;
            }
        }
        if derived.usage.is_some() {
            record.usage = derived.usage.clone();
        }
        if derived.report_status.is_some() {
            record.report = derived.report.clone();
            record.report_status = derived.report_status.clone();
            record.report_source = Some("native_structured_output".to_owned());
        }
        let pending_start = derived.turn_id.is_some()
            && record.operations.iter().any(|receipt| {
                receipt.action == "start" && receipt.phase == OperationPhase::Accepted
            });
        if *record == previous && !pending_start {
            return Ok(());
        }
        record.revision = record.revision.saturating_add(1);
        let outcome = record.outcome();
        if pending_start
            && let Some(receipt) = record.operations.iter_mut().find(|receipt| {
                receipt.action == "start" && receipt.phase == OperationPhase::Accepted
            })
        {
            receipt.phase = OperationPhase::Applied;
            receipt.outcome = outcome;
        }
        Ok(())
    })?;
    drop(apply_permit);
    if reconciled.deferred {
        return Ok(task_view_at_revision(&reconciled.record, after_revision));
    }
    record = reconciled.record;
    if after_revision == Some(record.revision) {
        return Ok(reconciled_task_view(&record, None, after_revision));
    }
    let evidence_permit = ensure_current_active_instance(&owner, session).await?;
    let evidence_ref = store_evidence_for_instance(&owner, session, &response);
    drop(evidence_permit);
    Ok(reconciled_task_view(
        &record,
        evidence_ref.as_ref(),
        after_revision,
    ))
}

fn reconciled_task_status(current: TaskStatus, derived: TaskStatus) -> TaskStatus {
    if current.is_terminal() && !derived.is_terminal() {
        current
    } else {
        derived
    }
}

struct DerivedThreadState {
    status: TaskStatus,
    turn_id: Option<String>,
    usage: Option<BTreeMap<String, u64>>,
    report: Option<Value>,
    report_status: Option<String>,
}

fn derive_thread_state(
    response: &Value,
    expected_turn_id: Option<&str>,
) -> Result<DerivedThreadState> {
    derive_thread_state_after(response, expected_turn_id, None)
}

fn derive_thread_state_after(
    response: &Value,
    expected_turn_id: Option<&str>,
    predecessor_turn_id: Option<&str>,
) -> Result<DerivedThreadState> {
    let thread = response
        .get("thread")
        .and_then(Value::as_object)
        .context("thread/read response is missing thread")?;
    let turns = thread
        .get("turns")
        .and_then(Value::as_array)
        .context("thread/read response is missing turns")?;
    let turn = expected_turn_id
        .and_then(|id| {
            turns
                .iter()
                .find(|turn| turn.get("id").and_then(Value::as_str) == Some(id))
        })
        .or_else(|| {
            if let Some(predecessor) = predecessor_turn_id {
                turns
                    .iter()
                    .position(|turn| turn.get("id").and_then(Value::as_str) == Some(predecessor))
                    .and_then(|index| turns.get(index + 1))
            } else {
                turns.last()
            }
        });
    let usage = thread
        .get("tokenUsage")
        .or_else(|| thread.get("usage"))
        .and_then(extract_usage)
        .or_else(|| turn.and_then(extract_usage_from_turn));
    let Some(turn) = turn else {
        let thread_status = thread
            .get("status")
            .and_then(|status| status.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Ok(DerivedThreadState {
            status: match thread_status {
                "idle" if predecessor_turn_id.is_some() => TaskStatus::ReconciliationRequired,
                "idle" => TaskStatus::Unknown,
                "active" => TaskStatus::Running,
                "systemError" => TaskStatus::Failed,
                _ => TaskStatus::Unknown,
            },
            turn_id: None,
            usage,
            report: None,
            report_status: None,
        });
    };
    let turn_id = turn.get("id").and_then(Value::as_str).map(str::to_owned);
    let status = turn
        .get("status")
        .and_then(Value::as_str)
        .and_then(task_status_from_str)
        .unwrap_or(TaskStatus::Unknown);
    let (report, report_status) = if status == TaskStatus::Completed {
        let text = turn
            .get("items")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .rev()
                    .find(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage"))
                    .and_then(|item| item.get("text"))
                    .and_then(Value::as_str)
            });
        let result = match text {
            None => (None, "missing_report"),
            Some(text) if text.len() > crate::report_contract::MAX_TASK_REPORT_BYTES => {
                (None, "oversized_report")
            }
            Some(text) => match serde_json::from_str::<Value>(text) {
                Ok(value) if crate::report_contract::validate_codex_native(&value) => {
                    (Some(value), "valid")
                }
                Ok(_) => (None, "invalid_report_schema"),
                Err(_) => (None, "invalid_json"),
            },
        };
        (result.0, Some(result.1.to_owned()))
    } else {
        (None, None)
    };
    Ok(DerivedThreadState {
        status,
        turn_id,
        usage,
        report,
        report_status,
    })
}

fn task_status_from_str(status: &str) -> Option<TaskStatus> {
    match status {
        "completed" => Some(TaskStatus::Completed),
        "interrupted" => Some(TaskStatus::Interrupted),
        "failed" => Some(TaskStatus::Failed),
        "inProgress" | "active" => Some(TaskStatus::Running),
        "waitingApproval" => Some(TaskStatus::WaitingApproval),
        _ => None,
    }
}

fn extract_usage_from_turn(turn: &Value) -> Option<BTreeMap<String, u64>> {
    turn.get("usage")
        .or_else(|| turn.get("tokenUsage"))
        .and_then(extract_usage)
}

enum EnsuredRuntime {
    Local(RpcClient),
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
    {
        let _guard = store.lock()?;
        let current = store.load_locked(session, record.task_id)?;
        anyhow::ensure!(
            current.continued_by_task_id.is_none(),
            "CODEX_CONTINUATION_CLAIMED: source conversation belongs to its successor"
        );
    }
    let _operation_permit = ensure_current_active_instance(&owner, session).await?;
    let (client, _) = spawn_initialized_client_with_binary_mode(
        session,
        Some(record.task_id),
        binary,
        true,
        Some(Arc::clone(&lease)),
    )
    .await?;
    let thread_id = record
        .thread_id
        .as_deref()
        .context("cannot resume Codex task without thread_id")?;
    let resume = request_for_instance(
        &client,
        &owner,
        session,
        "thread/resume",
        json!({
            "threadId": thread_id,
            "cwd": record.scope_cwd,
            "model": record.model,
            "approvalPolicy": CODEX_APPROVAL_POLICY,
            "approvalsReviewer": "user",
            "sandbox": "workspace-write",
            "runtimeWorkspaceRoots": [record.scope_cwd],
            "excludeTurns": true,
        }),
    )
    .await;
    if let Err(error) = resume {
        client.shutdown().await;
        return Err(error).context("Codex task could not resume its retained thread");
    }
    if let Err(error) = insert_runtime(session, record.task_id, store, client.clone(), lease).await
    {
        client.shutdown().await;
        return Err(error);
    }
    Ok(EnsuredRuntime::Local(client))
}

/// Read-only projection of the tasks this session instance owns.
/// Reconciliation stays in `task_get`: the list reports retained state
/// only, so it never touches runtimes or mutates records.
pub(crate) async fn task_list(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_list_with_store(args, session, &store)
}

fn task_list_with_store(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<Value> {
    let limit = task_list_limit(args)?;
    let (mut records, skipped) = store.list_owned(session)?;
    records.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    let total = records.len();
    let tasks: Vec<Value> = records.iter().take(limit).map(task_list_item).collect();
    Ok(json!({
        "backend": "codex",
        "tasks": tasks,
        "total": total,
        "skipped": skipped,
        "truncated": total > tasks.len(),
        "limit": limit,
    }))
}

fn task_list_item(record: &TaskRecord) -> Value {
    let mut item = task_view(record, None);
    item["backend"] = json!("codex");
    item["last_updated_at"] = json!(record.updated_at);
    item["pending_interaction"] =
        Summary::projection(record.pending_interaction.as_ref(), config::unix_time());
    item
}

fn task_list_limit(args: &Value) -> Result<usize> {
    match args.get("limit") {
        None => Ok(50),
        Some(value) => {
            let limit = value
                .as_u64()
                .context("task_list limit must be an integer")?;
            anyhow::ensure!(
                (1..=128).contains(&limit),
                "task_list limit must be 1..=128"
            );
            Ok(limit as usize)
        }
    }
}

pub(crate) async fn task_control(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    let binary = configured_codex_binary()?;
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
        "unsupported Codex task action"
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
    let _control_guard = TaskControlGuard::acquire(session, task_id)?;
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
    let thread_id = record
        .thread_id
        .clone()
        .context("Codex task requires reconciliation before control")?;
    let turn_id = record.turn_id.clone();
    let starts_new_turn =
        action == "steer" && record.previous_turn_id.is_some() && turn_id.is_none();

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
    let pending_summary_was_ready = client.pending_summary_ready_for(&record);
    let result = match action {
        "steer" => {
            if starts_new_turn {
                request_for_instance(&client, &owner, session, "turn/start", json!({
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": input.unwrap()}],
                    "model": record.model,
                    "effort": record.effort,
                    "cwd": record.scope_cwd,
                    "approvalPolicy": CODEX_APPROVAL_POLICY,
                    "outputSchema": crate::report_contract::codex_report_json_schema(),
                    "sandboxPolicy": {"type":"workspaceWrite", "writableRoots":[record.scope_cwd], "networkAccess":false}
                })).await
            } else {
                request_for_instance(
                    &client,
                    &owner,
                    session,
                    "turn/steer",
                    json!({
                        "threadId": thread_id,
                        "expectedTurnId": turn_id.as_deref().unwrap_or_default(),
                        "input": [{"type": "text", "text": input.unwrap()}],
                    }),
                )
                .await
            }
        }
        "interrupt" => {
            request_for_instance(
                &client,
                &owner,
                session,
                "turn/interrupt",
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id.as_deref().unwrap_or_default()
                }),
            )
            .await
        }
        "resume" => {
            request_for_instance(
                &client,
                &owner,
                session,
                "thread/read",
                json!({"threadId": thread_id, "includeTurns": true}),
            )
            .await
        }
        _ => unreachable!(),
    };
    if result.is_err() {
        let shutting_down = session_instance_is_closing(&owner);
        let record = apply_control_failure(store, session, task_id, operation_id, shutting_down)?;
        return Ok(task_view(&record, None));
    }

    let resumed = if action == "resume" {
        let response = match &result {
            Ok(response) => response,
            Err(_) => unreachable!("resume errors return above"),
        };
        match derive_thread_state(response, record.turn_id.as_deref()) {
            Ok(state) => Some(state),
            Err(_) => {
                let shutting_down = session_instance_is_closing(&owner);
                let record =
                    apply_control_failure(store, session, task_id, operation_id, shutting_down)?;
                return Ok(task_view(&record, None));
            }
        }
    } else {
        None
    };
    let apply_permit = ensure_current_active_instance(&owner, session).await?;
    record = store.update_if_instance_live(session, task_id, &owner, |record| {
        if record.status.is_terminal() {
            return Ok(());
        }
        if let Some(resumed) = resumed.as_ref() {
            record.status = reconciled_task_status(record.status, resumed.status);
            if resumed.turn_id.is_some() {
                record.turn_id = resumed.turn_id.clone();
                record.generation = record.generation.max(1);
            }
            if resumed.usage.is_some() {
                record.usage = resumed.usage.clone();
            }
        } else if starts_new_turn {
            let new_turn_id = result
                .as_ref()
                .ok()
                .and_then(|response| response.get("turn"))
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .context("turn/start response is missing turn.id")?;
            anyhow::ensure!(
                Some(new_turn_id) != record.previous_turn_id.as_deref(),
                "turn/start reused predecessor turn"
            );
            record.turn_id = Some(new_turn_id.to_owned());
            record.previous_turn_id = None;
            record.status = TaskStatus::Running;
        } else if !record.status.is_terminal() {
            record.generation = record.generation.saturating_add(1);
            record.status = if action == "interrupt" {
                TaskStatus::Interrupted
            } else {
                TaskStatus::Running
            };
        }
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
    if action == "steer" && pending_summary_was_ready {
        client.mark_pending_summary_ready(&record);
    }
    Ok(task_view(&record, None))
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

fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>> {
    args.get(key)
        .map(|value| {
            value
                .as_u64()
                .with_context(|| format!("{key} must be a non-negative integer"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    #[test]
    fn configured_multicall_codex_keeps_invocation_path() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("launcher");
        std::fs::write(&target, b"#!/bin/sh\n[ \"${0##*/}\" = codex ]\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let invocation = root.path().join("codex");
        std::os::unix::fs::symlink(&target, &invocation).unwrap();
        let validated = validate_configured_codex_binary(&invocation).unwrap();
        assert_eq!(validated, invocation);
        assert!(
            std::process::Command::new(validated)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            !std::process::Command::new(&target)
                .status()
                .unwrap()
                .success()
        );
        assert_ne!(std::fs::canonicalize(&invocation).unwrap(), invocation);
        assert!(validate_configured_codex_binary(Path::new("codex")).is_err());
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(validate_configured_codex_binary(&invocation).is_err());
    }

    use super::*;
    use std::cell::Cell;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::pin::Pin;
    use std::sync::{Arc, Barrier, mpsc};
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    struct NotifyOnReadPending<R> {
        inner: R,
        read_data: bool,
        pending_notice: Option<oneshot::Sender<()>>,
    }

    impl<R: AsyncRead + Unpin> AsyncRead for NotifyOnReadPending<R> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let filled_before = buffer.filled().len();
            let result = Pin::new(&mut this.inner).poll_read(cx, buffer);
            match &result {
                Poll::Ready(Ok(())) if buffer.filled().len() > filled_before => {
                    this.read_data = true;
                }
                Poll::Pending if this.read_data => {
                    if let Some(notice) = this.pending_notice.take() {
                        let _ = notice.send(());
                    }
                }
                _ => {}
            }
            result
        }
    }

    #[tokio::test]
    async fn bounded_json_line_read_keeps_prefix_when_cancelled_waiting_for_newline() {
        let (mut writer, raw_reader) = tokio::io::duplex(128);
        let (pending_notice, pending_notice_rx) = oneshot::channel();
        let reader = NotifyOnReadPending {
            inner: raw_reader,
            read_data: false,
            pending_notice: Some(pending_notice),
        };
        let mut reader = BufReader::new(reader);
        let mut partial_line = Vec::new();
        writer.write_all(b"{\"id\":").await.unwrap();

        {
            let read = read_bounded_json_line(&mut reader, &mut partial_line);
            tokio::pin!(read);
            tokio::select! {
                result = &mut read => panic!("read completed before the line suffix: {result:?}"),
                result = pending_notice_rx => result.expect("read did not consume the prefix"),
            }
        }

        assert_eq!(partial_line, b"{\"id\":");
        writer
            .write_all(b"7,\"result\":{\"ok\":true}}\n")
            .await
            .unwrap();
        let value = read_bounded_json_line(&mut reader, &mut partial_line)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(value, json!({"id": 7, "result": {"ok": true}}));
        assert!(partial_line.is_empty());
    }

    #[tokio::test]
    async fn bounded_json_line_read_rejects_overflowing_retained_prefix() {
        let (mut writer, raw_reader) = tokio::io::duplex(16);
        let mut reader = BufReader::new(raw_reader);
        let mut partial_line = vec![b' '; MAX_RPC_LINE_BYTES];
        writer.write_all(b"x\n").await.unwrap();
        let error = read_bounded_json_line(&mut reader, &mut partial_line)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("exceeds {MAX_RPC_LINE_BYTES} bytes"))
        );
    }

    #[tokio::test]
    async fn bounded_json_line_read_handles_multiple_lines_and_complete_eof() {
        let (mut writer, raw_reader) = tokio::io::duplex(128);
        let mut reader = BufReader::new(raw_reader);
        let mut partial_line = Vec::new();
        writer.write_all(b"{\"id\":1}\n{\"id\":2}").await.unwrap();
        drop(writer);

        assert_eq!(
            read_bounded_json_line(&mut reader, &mut partial_line)
                .await
                .unwrap(),
            Some(json!({"id": 1}))
        );
        assert_eq!(
            read_bounded_json_line(&mut reader, &mut partial_line)
                .await
                .unwrap(),
            Some(json!({"id": 2}))
        );
        assert_eq!(
            read_bounded_json_line(&mut reader, &mut partial_line)
                .await
                .unwrap(),
            None
        );
        assert!(partial_line.is_empty());
    }

    fn session(root: &Path, id: &str, yolo: bool) -> config::Session {
        let cwd = config::canonical_directory(root).unwrap();
        config::Session {
            id: id.to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1234,
            process_id: 5678,
            permission_mode: config::PermissionMode::from_legacy_yolo(yolo),
            grants: config::SessionGrants::default(),
        }
    }

    #[test]
    fn session_stop_monitor_retries_only_consecutive_unknown_observations() {
        let mut monitor = SessionStopMonitor::default();
        assert_eq!(
            monitor.observe(SessionMonitorObservation::Unknown(
                SessionMonitorUnknownKind::ActivityProbe
            )),
            SessionMonitorAction::Continue
        );
        assert_eq!(monitor.consecutive_unknown, 1);
        assert_eq!(
            monitor.observe(SessionMonitorObservation::Active),
            SessionMonitorAction::Continue
        );
        assert_eq!(monitor.consecutive_unknown, 0);

        for expected in 1..=10 {
            assert_eq!(
                monitor.observe(SessionMonitorObservation::Unknown(
                    SessionMonitorUnknownKind::MetadataRead
                )),
                SessionMonitorAction::Continue
            );
            assert_eq!(monitor.consecutive_unknown, expected);
        }

        assert_eq!(
            SessionStopMonitor::default().observe(SessionMonitorObservation::Inactive),
            SessionMonitorAction::Stop(SessionMonitorStopReason::Inactive)
        );
        assert_eq!(
            SessionStopMonitor::default().observe(SessionMonitorObservation::OwnerChanged),
            SessionMonitorAction::Stop(SessionMonitorStopReason::OwnerChanged)
        );
    }

    #[test]
    fn generated_session_stop_monitor_matches_reference_model() -> noprop::TestResult {
        crate::test_support::run(0x434f_4445_584d_4f4e, 1024, |ctx| {
            let observations = (0..noprop::sample_usize_in(ctx, 0..=64))
                .map(|_| noprop::sample_u8(ctx) % 5)
                .collect::<Vec<_>>();
            let mut monitor = SessionStopMonitor::default();
            let mut reference_unknown = 0_u8;

            for sample in observations {
                let (observation, expected) = match sample {
                    0 => {
                        reference_unknown = 0;
                        (
                            SessionMonitorObservation::Active,
                            SessionMonitorAction::Continue,
                        )
                    }
                    1 => (
                        SessionMonitorObservation::Inactive,
                        SessionMonitorAction::Stop(SessionMonitorStopReason::Inactive),
                    ),
                    2 => (
                        SessionMonitorObservation::OwnerChanged,
                        SessionMonitorAction::Stop(SessionMonitorStopReason::OwnerChanged),
                    ),
                    kind => {
                        let kind = if kind == 3 {
                            SessionMonitorUnknownKind::MetadataRead
                        } else {
                            SessionMonitorUnknownKind::ActivityProbe
                        };
                        reference_unknown = reference_unknown.saturating_add(1);
                        let expected = SessionMonitorAction::Continue;
                        (SessionMonitorObservation::Unknown(kind), expected)
                    }
                };

                let actual = monitor.observe(observation);
                assert_eq!(actual, expected);
                assert_eq!(monitor.consecutive_unknown, reference_unknown);
                if matches!(actual, SessionMonitorAction::Stop(_)) {
                    break;
                }
            }
            Ok(())
        })
    }

    fn fake_app_server(root: &Path, mode: &str) -> PathBuf {
        let path = root.join(format!("fake-app-server-{mode}"));
        let script = r##"#!/usr/bin/env python3
import json, os, sys
mode = os.path.basename(sys.argv[0]).split('fake-app-server-', 1)[-1]
thread_id = '0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa'
turn_id = '0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb'
for raw in sys.stdin:
    req = json.loads(raw)
    if req.get('method') == 'initialized':
        continue
    i = req.get('id')
    method = req.get('method')
    if mode == 'continuation':
        with open(os.path.join(os.path.dirname(sys.argv[0]), 'methods.log'), 'a') as log:
            log.write(method + '\n')
    if method == 'initialize':
        result = {'userAgent':'temote-mcp/0.153.4 (Ubuntu 24.4.0; x86_64) unknown (temote-mcp; __CLIENT_VERSION__)','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}
    elif method == 'model/list':
        if mode == 'model-fail':
            print(json.dumps({'id':i,'error':{'code':-1,'message':'model list unavailable'}}), flush=True)
            continue
        model = 'other-model' if mode == 'invalid-model' else 'gpt-5.6-luna'
        result = {'data':[{'model':model,'id':'luna','displayName':'Luna','description':'test','hidden':False,'isDefault':True,'defaultReasoningEffort':'high','supportedReasoningEfforts':[{'reasoningEffort':'low'},{'reasoningEffort':'medium'},{'reasoningEffort':'high'},{'reasoningEffort':'max'},{'reasoningEffort':'xhigh'}]}]}
    elif method == 'thread/start':
        if mode == 'thread-uncertain':
            print(json.dumps({'id':i,'error':{'code':-1,'message':'thread start response lost'}}), flush=True)
            continue
        if mode == 'reject-never' and req.get('params',{}).get('approvalPolicy') == 'never':
            print(json.dumps({'id':i,'error':{'code':-1,'message':'never approval policy rejected'}}), flush=True)
            continue
        if req.get('params',{}).get('sandbox') not in (None, 'workspace-write'):
            print(json.dumps({'id':i,'error':{'code':-1,'message':'invalid sandbox mode'}}), flush=True)
            continue
        result = {'thread':{'id':thread_id}}
    elif method == 'turn/start':
        if mode == 'reject-never' and req.get('params',{}).get('approvalPolicy') == 'never':
            print(json.dumps({'id':i,'error':{'code':-1,'message':'never approval policy rejected'}}), flush=True)
            continue
        if mode == 'approval':
            print(json.dumps({'id':'approval-1','method':'item/commandExecution/requestApproval','params':{'itemId':'item','startedAtMs':1,'threadId':thread_id,'turnId':turn_id}}), flush=True)
            approval = json.loads(sys.stdin.readline())
            if approval.get('result',{}).get('decision') != 'accept':
                print(json.dumps({'id':i,'error':{'code':-1,'message':'denied'}}), flush=True)
                continue
        result = {'turn':{'id':turn_id}}
    elif method == 'thread/resume':
        if mode == 'reject-never' and req.get('params',{}).get('approvalPolicy') == 'never':
            print(json.dumps({'id':i,'error':{'code':-1,'message':'never approval policy rejected'}}), flush=True)
            continue
        if req.get('params',{}).get('sandbox') not in (None, 'workspace-write'):
            print(json.dumps({'id':i,'error':{'code':-1,'message':'invalid sandbox mode'}}), flush=True)
            continue
        result = {'thread':{'id':thread_id}}
    elif method == 'thread/read':
        turn_status = 'inProgress' if mode == 'running-read' else 'completed'
        result = {'thread':{'id':thread_id,'status':{'type':'active' if mode == 'running-read' else 'idle'},'tokenUsage':{'inputTokens':4,'cachedInputTokens':1,'outputTokens':2,'reasoningOutputTokens':1,'totalTokens':6},'turns':[{'id':turn_id,'status':turn_status,'items':[{'type':'agentMessage','id':'m','text':'secret transcript marker'}]}]}}
    elif method == 'turn/steer':
        result = {'turnId':turn_id}
    elif method == 'turn/interrupt':
        result = {}
    else:
        print(json.dumps({'id':i,'error':{'code':-32601,'message':'unsupported'}}), flush=True)
        continue
    print(json.dumps({'id':i,'result':result}), flush=True)
"##;
        let script = script.replace("__CLIENT_VERSION__", APP_SERVER_CLIENT_VERSION);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn barrier_fake_app_server(
        root: &Path,
        blocked_method: &str,
    ) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
        let label = blocked_method.replace('/', "-");
        let path = root.join(format!("fake-app-server-barrier-{label}"));
        let entered = root.join(format!("{label}-entered"));
        let release = root.join(format!("{label}-release"));
        let turn_started = root.join(format!("{label}-turn-started"));
        let steer_sent = root.join(format!("{label}-steer-sent"));
        let python_string =
            |value: &Path| serde_json::to_string(&value.to_string_lossy().into_owned()).unwrap();
        let script = r##"#!/usr/bin/env python3
import json, os, sys, time
blocked_method = __BLOCKED_METHOD__
entered = __ENTERED__
release = __RELEASE__
turn_started = __TURN_STARTED__
steer_sent = __STEER_SENT__
thread_id = '0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa'
turn_id = '0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb'
for raw in sys.stdin:
    req = json.loads(raw)
    if req.get('method') == 'initialized':
        continue
    i = req.get('id')
    method = req.get('method')
    if method == 'initialize':
        result = {'userAgent':'temote-mcp/0.153.4 (Ubuntu 24.4.0; x86_64) unknown (temote-mcp; __CLIENT_VERSION__)','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'linux'}
    elif method == 'model/list':
        result = {'data':[{'model':'gpt-5.6-luna','id':'luna','displayName':'Luna','description':'test','hidden':False,'isDefault':True,'defaultReasoningEffort':'high','supportedReasoningEfforts':[{'reasoningEffort':'low'},{'reasoningEffort':'medium'},{'reasoningEffort':'high'},{'reasoningEffort':'max'},{'reasoningEffort':'xhigh'}]}]}
    elif method == blocked_method:
        open(entered, 'w').close()
        if method == 'turn/start':
            open(turn_started, 'w').close()
        while not os.path.exists(release):
            time.sleep(0.005)
        if method == 'turn/start':
            result = {'turn':{'id':turn_id}}
        else:
            result = {'thread':{'id':thread_id}}
    elif method == 'thread/start':
        result = {'thread':{'id':thread_id}}
    elif method == 'thread/resume':
        result = {'thread':{'id':thread_id}}
    elif method == 'turn/start':
        open(turn_started, 'w').close()
        result = {'turn':{'id':turn_id}}
    elif method == 'turn/steer':
        open(steer_sent, 'w').close()
        result = {'turnId':turn_id}
    elif method == 'turn/interrupt':
        result = {}
    elif method == 'thread/read':
        result = {'thread':{'id':thread_id,'status':{'type':'idle'},'turns':[{'id':turn_id,'status':'completed'}]}}
    else:
        print(json.dumps({'id':i,'error':{'code':-32601,'message':'unsupported'}}), flush=True)
        continue
    print(json.dumps({'id':i,'result':result}), flush=True)
"##
        .replace("__BLOCKED_METHOD__", &serde_json::to_string(blocked_method).unwrap())
        .replace("__ENTERED__", &python_string(&entered))
        .replace("__RELEASE__", &python_string(&release))
        .replace("__TURN_STARTED__", &python_string(&turn_started))
        .replace("__STEER_SENT__", &python_string(&steer_sent))
        .replace("__CLIENT_VERSION__", APP_SERVER_CLIENT_VERSION);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        (path, entered, release, turn_started, steer_sent)
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

    fn test_runtime_lease(root: &Path, task_id: Uuid) -> Arc<TaskRuntimeLease> {
        let store = TaskStore::new(root.join("test-runtime-locks"));
        Arc::new(
            store
                .try_acquire_runtime_lease(task_id)
                .unwrap()
                .expect("test task runtime lease is unavailable"),
        )
    }

    struct ChildGuard(Option<std::process::Child>);

    impl ChildGuard {
        fn new(child: std::process::Child) -> Self {
            Self(Some(child))
        }

        fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            let status = self.0.as_mut().unwrap().wait();
            if status.is_ok() {
                self.0.take();
            }
            status
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    async fn wait_for_marker(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fake app-server did not reach its barrier");
    }

    async fn wait_for_child_release(path: &Path) {
        // The parent can be delayed by the full test binary's other process
        // fixtures, so keep this bounded without making ordinary loaded runs
        // release the lease prematurely.
        tokio::time::timeout(Duration::from_secs(120), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cross-process test child did not receive its release marker");
    }

    fn recording_client(methods: Arc<Mutex<Vec<String>>>) -> RpcClient {
        let (commands, mut receiver) = tokio::sync::mpsc::channel(8);
        let actor = tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    ClientCommand::Request { method, reply, .. } => {
                        methods.lock().unwrap().push(method.to_owned());
                        let result = match method {
                            "thread/start" => json!({"thread":{"id":"thread"}}),
                            "turn/start" => json!({"turn":{"id":"turn"}}),
                            _ => json!({}),
                        };
                        let _ = reply.send(Ok(result));
                    }
                    ClientCommand::Notify { .. } => {}
                    ClientCommand::Shutdown => break,
                }
            }
        });
        RpcClient {
            tx: commands,
            actor: Arc::new(Mutex::new(Some(actor))),
            connected: Arc::new(AtomicBool::new(true)),
            pending_approvals: Arc::new(AtomicU64::new(0)),
            pending_summary_binding: Arc::new(Mutex::new(None)),
            runtime_instance_id: Arc::new(Mutex::new(None)),
        }
    }

    fn reconciliation_client(
        response: Arc<Mutex<std::result::Result<Value, String>>>,
        methods: Arc<Mutex<Vec<String>>>,
        mut barrier: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    ) -> RpcClient {
        let (commands, mut receiver) = tokio::sync::mpsc::channel(8);
        let actor = tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    ClientCommand::Request { method, reply, .. } => {
                        methods.lock().unwrap().push(method.to_owned());
                        assert_eq!(method, "thread/read");
                        let result = response.lock().unwrap().clone();
                        if let Some((entered, release)) = barrier.take() {
                            let _ = entered.send(());
                            let _ = release.await;
                        }
                        let _ = reply.send(result);
                    }
                    ClientCommand::Notify { .. } => {}
                    ClientCommand::Shutdown => break,
                }
            }
        });
        RpcClient {
            tx: commands,
            actor: Arc::new(Mutex::new(Some(actor))),
            connected: Arc::new(AtomicBool::new(true)),
            pending_approvals: Arc::new(AtomicU64::new(0)),
            pending_summary_binding: Arc::new(Mutex::new(None)),
            runtime_instance_id: Arc::new(Mutex::new(None)),
        }
    }

    fn blocking_shutdown_client(entered: PathBuf, release: PathBuf) -> RpcClient {
        let (commands, mut receiver) = tokio::sync::mpsc::channel(8);
        let actor = tokio::spawn(async move {
            'commands: while let Some(command) = receiver.recv().await {
                match command {
                    ClientCommand::Request { method, reply, .. } => {
                        assert_eq!(method, "turn/start");
                        std::fs::write(&entered, b"entered").unwrap();
                        loop {
                            if release.exists() {
                                let _ = reply.send(Ok(json!({
                                    "turn": {"id": "turn"}
                                })));
                                break;
                            }
                            match tokio::time::timeout(Duration::from_millis(5), receiver.recv())
                                .await
                            {
                                Ok(Some(ClientCommand::Shutdown)) => {
                                    while !release.exists() {
                                        tokio::time::sleep(Duration::from_millis(5)).await;
                                    }
                                    let _ = reply
                                        .send(Err("fake Codex child shutdown released".to_owned()));
                                    break 'commands;
                                }
                                Ok(Some(ClientCommand::Notify { .. })) => {}
                                Ok(Some(ClientCommand::Request { reply, .. })) => {
                                    let _ = reply.send(Err(
                                        "fake Codex child accepts one request".to_owned()
                                    ));
                                }
                                Ok(None) => break 'commands,
                                Err(_) => {}
                            }
                        }
                    }
                    ClientCommand::Notify { .. } => {}
                    ClientCommand::Shutdown => break,
                }
            }
        });
        RpcClient {
            tx: commands,
            actor: Arc::new(Mutex::new(Some(actor))),
            connected: Arc::new(AtomicBool::new(true)),
            pending_approvals: Arc::new(AtomicU64::new(0)),
            pending_summary_binding: Arc::new(Mutex::new(None)),
            runtime_instance_id: Arc::new(Mutex::new(None)),
        }
    }

    fn task_record(
        owner: &config::Session,
        task_id: Uuid,
        status: TaskStatus,
        revision: u64,
        thread_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> TaskRecord {
        let now = config::unix_time();
        TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(owner),
            scope_cwd: owner.cwd.clone(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status,
            revision,
            generation: u64::from(thread_id.is_some()),
            thread_id: thread_id.map(str::to_owned),
            continued_from_task_id: None,
            continued_by_task_id: None,
            continued_by_request_fingerprint: None,
            turn_id: turn_id.map(str::to_owned),
            previous_turn_id: None,
            usage: None,
            report: None,
            report_source: None,
            report_status: None,
            pending_interaction: None,
            created_at: now,
            updated_at: now,
            verification: None,
            delivery: None,
            operations: Vec::new(),
            operation_tombstones: Vec::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn prompt_item_binding_requires_exact_task_turn_and_scope() {
        let root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "prompt-binding", false);
        let task_id = Uuid::new_v4();
        let mut record = task_record(
            &owner,
            task_id,
            TaskStatus::Running,
            1,
            Some("thread-1"),
            Some("turn-1"),
        );
        let params = json!({"threadId":"thread-1", "turnId":"turn-1", "startedAtMs":42,
            "item":{"type":"userMessage", "id":"item-1",
                "content":[{"type":"text", "text":"instruction"}]}});
        let mut candidate =
            crate::codex_prompt_observer::parse_item_started("item/started", Some(&params))
                .unwrap();
        let binding = prompt_notification_binding(&owner, &record, &candidate).unwrap();
        assert_eq!(binding.task_id, task_id.to_string());
        assert_eq!(
            binding.execution_id,
            outcome::execution_id(task_id, record.generation).to_string()
        );
        assert_ne!(binding.execution_id, candidate.turn_id);
        candidate.turn_id = "other-turn".into();
        assert!(prompt_notification_binding(&owner, &record, &candidate).is_none());
        candidate.turn_id = "turn-1".into();
        candidate.thread_id = "other-thread".into();
        assert!(prompt_notification_binding(&owner, &record, &candidate).is_none());
        candidate.thread_id = "thread-1".into();
        record.scope_cwd = root.path().join("other");
        assert!(prompt_notification_binding(&owner, &record, &candidate).is_none());
        record.scope_cwd = owner.cwd.clone();
        record.owner.started_at += 1;
        assert!(prompt_notification_binding(&owner, &record, &candidate).is_none());
    }

    fn start_receipt(
        operation_id: Uuid,
        request_fingerprint: Uuid,
        phase: OperationPhase,
        outcome: OperationOutcome,
    ) -> OperationReceipt {
        OperationReceipt {
            operation_id,
            request_fingerprint,
            action: "start".to_owned(),
            phase,
            outcome,
        }
    }

    #[test]
    fn reconciliation_never_regresses_a_terminal_task() {
        assert_eq!(
            reconciled_task_status(TaskStatus::Completed, TaskStatus::Running),
            TaskStatus::Completed
        );
        assert_eq!(
            reconciled_task_status(TaskStatus::Interrupted, TaskStatus::WaitingApproval),
            TaskStatus::Interrupted
        );
        assert_eq!(
            reconciled_task_status(TaskStatus::Failed, TaskStatus::Unknown),
            TaskStatus::Failed
        );
        assert_eq!(
            reconciled_task_status(TaskStatus::Running, TaskStatus::Completed),
            TaskStatus::Completed
        );
    }

    #[test]
    fn runtime_lease_is_exclusive_per_task_and_released_on_drop() {
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let first_task = Uuid::new_v4();
        let second_task = Uuid::new_v4();

        let first_lease = store
            .try_acquire_runtime_lease(first_task)
            .unwrap()
            .expect("first runtime lease was not acquired");
        assert!(
            store
                .try_acquire_runtime_lease(first_task)
                .unwrap()
                .is_none(),
            "the same task acquired a second live runtime lease"
        );
        let second_lease = store
            .try_acquire_runtime_lease(second_task)
            .unwrap()
            .expect("independent task runtime lease was not acquired");

        drop(first_lease);
        assert!(
            store
                .try_acquire_runtime_lease(first_task)
                .unwrap()
                .is_some(),
            "runtime lease was not released when its owner dropped"
        );
        drop(second_lease);
    }

    #[test]
    fn terminal_task_runtime_lease_defers_owner_cleanup_without_rewriting_terminal_state() {
        let workspace = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner_session = session(workspace.path(), "terminal-lease", true);
        let owner = SessionInstance::from_session(&owner_session);
        let task_id = Uuid::new_v4();
        let mut record = task_record(
            &owner_session,
            task_id,
            TaskStatus::Completed,
            30,
            Some("terminal-thread"),
            Some("terminal-turn"),
        );
        record.owner = owner.clone();
        store.save(&record).unwrap();

        let lease = store
            .try_acquire_runtime_lease(task_id)
            .unwrap()
            .expect("terminal runtime lease was not acquired");
        let deferred = store.finalize_owner(&owner).unwrap();
        assert_eq!(deferred.finalized, 0);
        assert!(deferred.deferred);
        assert_eq!(store.load(&owner_session, task_id).unwrap(), record);

        drop(lease);
        let completed = store.finalize_owner(&owner).unwrap();
        assert_eq!(completed.finalized, 0);
        assert!(!completed.deferred);
        assert_eq!(store.load(&owner_session, task_id).unwrap(), record);
    }

    #[test]
    fn task_store_is_scope_and_full_session_instance_bound_and_prompt_free() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(root.path(), "owner", true);
        let operation_id = Uuid::new_v4();
        let task_id = task_id_for_operation(&owner, operation_id).unwrap();
        let now = config::unix_time();
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(&owner),
            scope_cwd: owner.cwd.clone(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status: TaskStatus::Accepted,
            revision: 1,
            generation: 0,
            thread_id: None,
            continued_from_task_id: None,
            continued_by_task_id: None,
            continued_by_request_fingerprint: None,
            turn_id: None,
            previous_turn_id: None,
            usage: None,
            report: None,
            report_source: None,
            report_status: None,
            pending_interaction: None,
            created_at: now,
            updated_at: now,
            verification: None,
            delivery: None,
            operations: vec![OperationReceipt {
                operation_id,
                request_fingerprint: fingerprint(&json!({"task":"prompt-secret-marker"})).unwrap(),
                action: "start".to_owned(),
                phase: OperationPhase::Accepted,
                outcome: OperationOutcome {
                    status: TaskStatus::Accepted,
                    revision: 1,
                    generation: 0,
                    thread_id: None,
                    turn_id: None,
                },
            }],
            operation_tombstones: Vec::new(),
        };
        store.save(&record).unwrap();
        let bytes = std::fs::read(store.path(task_id)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("prompt-secret-marker"));
        assert_eq!(store.load(&owner, task_id).unwrap(), record);

        let mut restarted = owner.clone();
        restarted.process_id += 1;
        assert!(store.load(&restarted, task_id).is_err());
        let other_root = tempfile::tempdir().unwrap();
        let other = session(other_root.path(), "owner", true);
        assert!(store.load(&other, task_id).is_err());
    }

    #[tokio::test]
    async fn post_acceptance_admission_failure_is_terminal_and_exact_retry_does_not_reapply() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(root.path(), "clone-admission", false);
        let operation_id = Uuid::new_v4();
        let args = json!({
            "operation_id": operation_id,
            "task": "clone one admitted repository",
            "model": "gpt-5.6-luna",
            "effort": "high"
        });
        let missing_binary = root.path().join("must-not-run");

        let error = task_start_with_store_binary_and_admission(
            &args,
            &owner,
            &store,
            &missing_binary,
            &TaskStartOrigin::Generic,
            || anyhow::bail!("destination raced"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("before delegated side effect"));

        let replay = task_start_with_store_binary_and_admission(
            &args,
            &owner,
            &store,
            &missing_binary,
            &TaskStartOrigin::Generic,
            || panic!("exact retained retry must bypass filesystem admission"),
        )
        .await
        .unwrap();
        assert_eq!(replay["status"], "failed");
        assert!(replay["task_id"].is_string());
    }

    #[tokio::test]
    async fn task_start_origin_conflicts_before_admission_in_both_cross_tool_directions() {
        let cases = [
            (
                "generic-to-clone",
                TaskStartOrigin::Generic,
                TaskStartOrigin::repository_clone_bare("src", "src/source", "repo.git"),
            ),
            (
                "clone-to-generic",
                TaskStartOrigin::repository_clone_bare("src", "src/source", "repo.git"),
                TaskStartOrigin::Generic,
            ),
            (
                "named-root-alias",
                TaskStartOrigin::repository_clone_bare("src", "src/source", "repo.git"),
                TaskStartOrigin::repository_clone_bare("work", "work/source", "repo.git"),
            ),
        ];

        for (label, first_origin, conflicting_origin) in cases {
            let root = tempfile::tempdir().unwrap();
            let store_root = tempfile::tempdir().unwrap();
            let store = TaskStore::new(store_root.path().join("tasks"));
            let owner = session(root.path(), label, false);
            let operation_id = Uuid::new_v4();
            let args = json!({
                "operation_id": operation_id,
                // `src/source` and `work/source` both lower to this exact task
                // when the named roots alias the same canonical directory.
                "task": "clone ./source into ./repo.git",
                "model": "gpt-5.6-luna",
                "effort": "high"
            });
            let missing_binary = root.path().join("must-not-run");

            let first = match &first_origin {
                TaskStartOrigin::Generic => {
                    task_start_with_store_and_binary(&args, &owner, &store, &missing_binary).await
                }
                TaskStartOrigin::RepositoryCloneBare { .. }
                | TaskStartOrigin::OpenCodeWorkspaceCheck { .. } => {
                    task_start_with_store_binary_and_admission(
                        &args,
                        &owner,
                        &store,
                        &missing_binary,
                        &first_origin,
                        || Ok(()),
                    )
                    .await
                }
            }
            .unwrap();
            assert_eq!(first["status"], "retryable_failed", "{label}");

            let task_id = task_id_for_operation(&owner, operation_id).unwrap();
            let before = store.load(&owner, task_id).unwrap();
            let admissions = Cell::new(0usize);
            let conflict = match &conflicting_origin {
                TaskStartOrigin::Generic => {
                    task_start_with_store_and_binary(&args, &owner, &store, &missing_binary).await
                }
                TaskStartOrigin::RepositoryCloneBare { .. }
                | TaskStartOrigin::OpenCodeWorkspaceCheck { .. } => {
                    task_start_with_store_binary_and_admission(
                        &args,
                        &owner,
                        &store,
                        &missing_binary,
                        &conflicting_origin,
                        || {
                            admissions.set(admissions.get() + 1);
                            Ok(())
                        },
                    )
                    .await
                }
            };
            let error = conflict.unwrap_err();

            assert!(
                error.to_string().contains("OPERATION_CONFLICT"),
                "{label}: {error:#}"
            );
            assert_eq!(admissions.get(), 0, "{label}");
            assert_eq!(store.load(&owner, task_id).unwrap(), before, "{label}");
        }
    }

    #[test]
    fn private_workspace_origin_fences_parent_workspace_action_and_epoch() {
        let task_id = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let workspace = Uuid::new_v4();
        let base = TaskStartOrigin::opencode_workspace_check(parent, workspace, "test", 1);
        let fingerprint_for = |origin: &TaskStartOrigin| {
            task_start_fingerprint(task_id, "fixed task", "gpt-5.6-luna", "high", origin).unwrap()
        };
        let expected = fingerprint_for(&base);
        assert_ne!(expected, fingerprint_for(&TaskStartOrigin::Generic));
        for changed in [
            TaskStartOrigin::opencode_workspace_check(Uuid::new_v4(), workspace, "test", 1),
            TaskStartOrigin::opencode_workspace_check(parent, Uuid::new_v4(), "test", 1),
            TaskStartOrigin::opencode_workspace_check(parent, workspace, "build", 1),
            TaskStartOrigin::opencode_workspace_check(parent, workspace, "test", 2),
        ] {
            assert_ne!(expected, fingerprint_for(&changed));
        }
    }

    #[tokio::test]
    async fn exact_clone_origin_retries_pre_thread_then_replays_without_readmission() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(root.path(), "clone-origin-retry", false);
        let operation_id = Uuid::new_v4();
        let args = json!({
            "operation_id": operation_id,
            "task": "clone ./source into ./repo.git",
            "model": "gpt-5.6-luna",
            "effort": "high"
        });
        let origin = TaskStartOrigin::repository_clone_bare("src", "src/source", "repo.git");

        let first = task_start_with_store_binary_and_admission(
            &args,
            &owner,
            &store,
            &root.path().join("missing-codex"),
            &origin,
            || Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(first["status"], "retryable_failed");
        assert!(
            task_start_replay_if_retained_with_store(&args, &owner, &origin, &store)
                .unwrap()
                .is_none(),
            "pre-thread failures must repeat admission before retry"
        );

        let second = task_start_with_store_binary_and_admission(
            &args,
            &owner,
            &store,
            &fake_app_server(root.path(), "ok"),
            &origin,
            || Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(second["status"], "running");
        let preflight = task_start_replay_if_retained_with_store(&args, &owner, &origin, &store)
            .unwrap()
            .unwrap();
        assert_eq!(preflight["task_id"], second["task_id"]);
        assert_eq!(preflight["status"], "running");

        let replay = task_start_with_store_binary_and_admission(
            &args,
            &owner,
            &store,
            &root.path().join("must-not-run"),
            &origin,
            || panic!("an exact retained clone retry must not reapply admission"),
        )
        .await
        .unwrap();
        assert_eq!(replay["task_id"], second["task_id"]);
        assert_eq!(replay["status"], "running");

        remove_session_with_store(&owner, &store).await.unwrap();
    }

    #[tokio::test]
    async fn legacy_generic_receipt_reloads_but_cannot_be_promoted_to_clone_origin() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store_path = store_root.path().join("tasks");
        let store = TaskStore::new(store_path.clone());
        let owner = session(root.path(), "legacy-generic-origin", false);
        let operation_id = Uuid::new_v4();
        let args = json!({
            "operation_id": operation_id,
            "task": "clone ./source into ./repo.git",
            "model": "gpt-5.6-luna",
            "effort": "high"
        });
        let task_id = task_id_for_operation(&owner, operation_id).unwrap();
        let legacy_fingerprint = fingerprint(&json!({
            "kind": "start",
            "task_id": task_id,
            "task": args["task"],
            "model": args["model"],
            "effort": args["effort"],
        }))
        .unwrap();
        let mut record = task_record(
            &owner,
            task_id,
            TaskStatus::Completed,
            2,
            Some("legacy-thread"),
            Some("legacy-turn"),
        );
        record.effort = "high".to_owned();
        record.operations.push(start_receipt(
            operation_id,
            legacy_fingerprint,
            OperationPhase::Applied,
            record.outcome(),
        ));
        store.save(&record).unwrap();
        drop(store);

        let reloaded = TaskStore::new(store_path);
        let generic = task_start_with_store_binary_and_admission(
            &args,
            &owner,
            &reloaded,
            &root.path().join("must-not-run"),
            &TaskStartOrigin::Generic,
            || panic!("a retained generic retry must not reapply admission"),
        )
        .await
        .unwrap();
        assert_eq!(generic["status"], "completed");

        let before = reloaded.load(&owner, task_id).unwrap();
        let clone_origin = TaskStartOrigin::repository_clone_bare("src", "src/source", "repo.git");
        let error = task_start_with_store_binary_and_admission(
            &args,
            &owner,
            &reloaded,
            &root.path().join("must-not-run"),
            &clone_origin,
            || panic!("an ambiguous legacy receipt must not become a clone receipt"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("OPERATION_CONFLICT"));
        assert_eq!(reloaded.load(&owner, task_id).unwrap(), before);

        let persisted = std::fs::read(reloaded.path(task_id)).unwrap();
        let persisted = String::from_utf8_lossy(&persisted);
        assert!(!persisted.contains("repository_clone_bare"));
        assert!(!persisted.contains("src/source"));
        assert!(!persisted.contains("repo.git"));
    }

    #[test]
    fn generated_clone_origin_preflight_matches_reference_and_is_read_only() -> noprop::TestResult {
        const STATUSES: [TaskStatus; 4] = [
            TaskStatus::Accepted,
            TaskStatus::Running,
            TaskStatus::RetryableFailed,
            TaskStatus::Completed,
        ];
        const PHASES: [OperationPhase; 3] = [
            OperationPhase::Accepted,
            OperationPhase::Applied,
            OperationPhase::RetryableFailed,
        ];

        crate::test_support::run(0x434c_4f4e_4f52_4947, 8, |ctx| {
            let root = tempfile::tempdir().unwrap();
            let store_root = tempfile::tempdir().unwrap();
            let store = TaskStore::new(store_root.path().join("tasks"));
            let nonce = noprop::sample_u64(ctx);
            let component = crate::test_support::safe_component(ctx);
            let owner = session(root.path(), &format!("origin-model-{nonce:016x}"), false);
            let origin = TaskStartOrigin::repository_clone_bare(
                "src",
                &format!("src/{component}"),
                &format!("{component}.git"),
            );

            let missing_operation = Uuid::from_u128((u128::from(nonce) << 64) | 1);
            let missing_args = json!({
                "operation_id": missing_operation,
                "task": format!("clone ./{component} into ./{component}.git"),
                "model": "gpt-5.6-luna",
                "effort": "high"
            });
            assert!(
                task_start_replay_if_retained_with_store(&missing_args, &owner, &origin, &store,)
                    .unwrap()
                    .is_none(),
                "an absent receipt is not retained"
            );

            let mut case_index = 2u128;
            for status in STATUSES {
                for phase in PHASES {
                    for has_thread in [false, true] {
                        for identity_matches in [false, true] {
                            for action_is_start in [false, true] {
                                let operation_id =
                                    Uuid::from_u128((u128::from(nonce) << 64) | case_index);
                                case_index += 1;
                                let args = json!({
                                    "operation_id": operation_id,
                                    "task": missing_args["task"],
                                    "model": "gpt-5.6-luna",
                                    "effort": "high"
                                });
                                let task_id = task_id_for_operation(&owner, operation_id).unwrap();
                                let expected_fingerprint = task_start_fingerprint(
                                    task_id,
                                    args["task"].as_str().unwrap(),
                                    args["model"].as_str().unwrap(),
                                    args["effort"].as_str().unwrap(),
                                    &origin,
                                )
                                .unwrap();
                                let stored_fingerprint = if identity_matches {
                                    expected_fingerprint
                                } else {
                                    Uuid::from_u128(expected_fingerprint.as_u128() ^ 1)
                                };
                                let thread_id = has_thread.then_some("thread");
                                let mut record = task_record(
                                    &owner,
                                    task_id,
                                    status,
                                    2,
                                    thread_id,
                                    thread_id.map(|_| "turn"),
                                );
                                record.effort = "high".to_owned();
                                let mut receipt = start_receipt(
                                    operation_id,
                                    stored_fingerprint,
                                    phase,
                                    record.outcome(),
                                );
                                if !action_is_start {
                                    receipt.action = "resume".to_owned();
                                }
                                record.operations.push(receipt);
                                store.save(&record).unwrap();
                                let before_bytes = std::fs::read(store.path(task_id)).unwrap();

                                let actual = task_start_replay_if_retained_with_store(
                                    &args, &owner, &origin, &store,
                                );
                                let actual_category = match &actual {
                                    Ok(Some(_)) => "retained",
                                    Ok(None) => "absent",
                                    Err(_) => "error",
                                };
                                let retryable_before_thread = identity_matches
                                    && action_is_start
                                    && phase == OperationPhase::RetryableFailed
                                    && status == TaskStatus::RetryableFailed
                                    && !has_thread;
                                match (identity_matches, retryable_before_thread, actual) {
                                    (false, _, Err(error)) => assert!(
                                        error.to_string().contains("OPERATION_CONFLICT"),
                                        "unexpected identity error category"
                                    ),
                                    (true, true, Ok(None)) => {}
                                    (true, false, Ok(Some(_))) => {}
                                    (_, _, _) => panic!(
                                        "preflight diverged from reference: status={status:?} phase={phase:?} thread={has_thread} identity={identity_matches} action_start={action_is_start} actual={actual_category}"
                                    ),
                                }
                                assert_eq!(store.load(&owner, task_id).unwrap(), record);
                                assert_eq!(
                                    std::fs::read(store.path(task_id)).unwrap(),
                                    before_bytes
                                );
                            }
                        }
                    }
                }
            }

            let corrupt_operation = Uuid::from_u128((u128::from(nonce) << 64) | case_index);
            let corrupt_args = json!({
                "operation_id": corrupt_operation,
                "task": missing_args["task"],
                "model": "gpt-5.6-luna",
                "effort": "high"
            });
            let corrupt_task = task_id_for_operation(&owner, corrupt_operation).unwrap();
            std::fs::write(store.path(corrupt_task), b"not-json").unwrap();
            let before_corrupt = std::fs::read(store.path(corrupt_task)).unwrap();
            assert!(
                task_start_replay_if_retained_with_store(&corrupt_args, &owner, &origin, &store,)
                    .is_err(),
                "a corrupt record must fail closed"
            );
            assert_eq!(
                std::fs::read(store.path(corrupt_task)).unwrap(),
                before_corrupt
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn get_and_control_foreign_session_denied() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let (_ha, session_a) = active_test_session(
            root.path(),
            &format!("foreign-owner-{}", Uuid::new_v4()),
            false,
        )
        .await;
        let (_hb, session_b) = active_test_session(
            root.path(),
            &format!("foreign-other-{}", Uuid::new_v4()),
            false,
        )
        .await;
        let task_id = Uuid::new_v4();
        let now = config::unix_time();
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(&session_a),
            scope_cwd: config::canonical_directory(&session_a.cwd).unwrap(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status: TaskStatus::Running,
            revision: 1,
            generation: 1,
            thread_id: Some("0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa".to_owned()),
            continued_from_task_id: None,
            continued_by_task_id: None,
            continued_by_request_fingerprint: None,
            turn_id: Some("0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb".to_owned()),
            previous_turn_id: None,
            usage: None,
            report: None,
            report_source: None,
            report_status: None,
            pending_interaction: None,
            created_at: now,
            updated_at: now,
            verification: None,
            delivery: None,
            operations: Vec::new(),
            operation_tombstones: Vec::new(),
        };
        store.save(&record).unwrap();

        // Reads and controls from a different session fail at the task
        // store's ownership check, before any app-server runtime spawns.
        let error = task_get_with_store_and_binary(
            &json!({"task_id": task_id}),
            &session_b,
            &store,
            Path::new("codex"),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("CODEX_TASK_NOT_FOUND"));

        let error = task_control_with_store_and_binary(
            &json!({
                "task_id": task_id,
                "operation_id": Uuid::new_v4(),
                "action": "interrupt",
            }),
            &session_b,
            &store,
            Path::new("codex"),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("CODEX_TASK_NOT_FOUND"));
    }

    #[test]
    fn accepted_operation_replay_requires_reconciliation_and_conflicts_on_change() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "replay", true);
        let operation_id = Uuid::new_v4();
        let task_id = task_id_for_operation(&session, operation_id).unwrap();
        let fp = fingerprint(&json!({"same":true})).unwrap();
        let now = config::unix_time();
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(&session),
            scope_cwd: session.cwd.clone(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status: TaskStatus::Accepted,
            revision: 1,
            generation: 0,
            thread_id: None,
            continued_from_task_id: None,
            continued_by_task_id: None,
            continued_by_request_fingerprint: None,
            turn_id: None,
            previous_turn_id: None,
            usage: None,
            report: None,
            report_source: None,
            report_status: None,
            pending_interaction: None,
            created_at: now,
            updated_at: now,
            verification: None,
            delivery: None,
            operations: vec![OperationReceipt {
                operation_id,
                request_fingerprint: fp,
                action: "start".to_owned(),
                phase: OperationPhase::Accepted,
                outcome: OperationOutcome {
                    status: TaskStatus::Accepted,
                    revision: 1,
                    generation: 0,
                    thread_id: None,
                    turn_id: None,
                },
            }],
            operation_tombstones: Vec::new(),
        };
        let replay = replay_operation(&record, operation_id, fp).unwrap();
        assert_eq!(replay["status"], "reconciliation_required");
        assert!(replay_operation(&record, operation_id, Uuid::new_v4()).is_err());
    }

    #[tokio::test]
    async fn session_runtime_cleanup_is_instance_fenced_and_waits_for_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let old = session(root.path(), "runtime-owner", false);
        let mut replacement = old.clone();
        replacement.started_at += 1;
        replacement.process_id += 1;
        let old_task = Uuid::new_v4();
        let replacement_task = Uuid::new_v4();

        let (old_commands, mut old_receiver) = tokio::sync::mpsc::channel(1);
        let (old_stopped, old_stopped_receiver) = oneshot::channel();
        let old_actor = tokio::spawn(async move {
            if matches!(old_receiver.recv().await, Some(ClientCommand::Shutdown)) {
                let _ = old_stopped.send(());
            }
        });
        let old_client = RpcClient {
            tx: old_commands,
            actor: Arc::new(Mutex::new(Some(old_actor))),

            connected: Arc::new(AtomicBool::new(true)),
            pending_approvals: Arc::new(AtomicU64::new(0)),
            pending_summary_binding: Arc::new(Mutex::new(None)),
            runtime_instance_id: Arc::new(Mutex::new(None)),
        };

        let (replacement_commands, mut replacement_receiver) = tokio::sync::mpsc::channel(1);
        let replacement_actor = tokio::spawn(async move {
            let _ = replacement_receiver.recv().await;
        });
        let replacement_client = RpcClient {
            tx: replacement_commands,
            actor: Arc::new(Mutex::new(Some(replacement_actor))),

            connected: Arc::new(AtomicBool::new(true)),
            pending_approvals: Arc::new(AtomicU64::new(0)),
            pending_summary_binding: Arc::new(Mutex::new(None)),
            runtime_instance_id: Arc::new(Mutex::new(None)),
        };
        let old_lease = test_runtime_lease(root.path(), old_task);
        let replacement_lease = test_runtime_lease(root.path(), replacement_task);

        runtimes().lock().unwrap().insert(
            old_task,
            RuntimeHandle {
                client: old_client,
                owner: SessionInstance::from_session(&old),
                scope: old.cwd.clone(),
                instance_id: Uuid::new_v4(),
                started_at: Instant::now(),
                _lease: old_lease,
            },
        );
        runtimes().lock().unwrap().insert(
            replacement_task,
            RuntimeHandle {
                client: replacement_client,
                owner: SessionInstance::from_session(&replacement),
                scope: replacement.cwd.clone(),
                instance_id: Uuid::new_v4(),
                started_at: Instant::now(),
                _lease: replacement_lease,
            },
        );

        remove_session(&old).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), old_stopped_receiver)
            .await
            .unwrap()
            .unwrap();
        assert!(runtime_for(&old, old_task).is_none());
        assert!(runtime_for(&replacement, replacement_task).is_some());

        remove_session(&replacement).await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_waits_for_a_blocked_turn_start_request_to_drain() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("drain-turn-start-{}", Uuid::new_v4());
        let (handle, owner) = active_test_session(root.path(), &id, true).await;
        let entered = root.path().join("turn-start-entered");
        let release = root.path().join("turn-start-release");
        let client = blocking_shutdown_client(entered.clone(), release.clone());
        let owner_instance = SessionInstance::from_session(&owner);
        let operation = tokio::spawn({
            let client = client.clone();
            let owner = owner.clone();
            let owner_instance = owner_instance.clone();
            async move {
                request_for_instance(&client, &owner_instance, &owner, "turn/start", json!({}))
                    .await
            }
        });

        wait_for_marker(&entered).await;
        handle.shutdown().await.unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let mut removal = tokio::spawn({
            let owner = owner.clone();
            async move { remove_session_with_store(&owner, &store).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut removal)
                .await
                .is_err(),
            "session cleanup returned before the in-flight turn/start child shutdown"
        );

        std::fs::write(&release, b"release").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), operation)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(5), &mut removal)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!session_instance_is_closing(&owner_instance));
        assert!(
            runtimes()
                .lock()
                .unwrap()
                .values()
                .all(|runtime| { runtime.owner != owner_instance })
        );
    }

    #[tokio::test]
    async fn stop_during_turn_start_waits_for_inflight_child_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("task-drain-turn-start-{}", Uuid::new_v4());
        let (handle, owner) = active_test_session(root.path(), &id, true).await;
        let store = TaskStore::new(store_root.path().join("tasks"));
        let (binary, _entered, release, turn_started, _) =
            barrier_fake_app_server(root.path(), "turn/start");
        let operation_id = Uuid::new_v4();
        let args = json!({
            "operation_id": operation_id,
            "task": "stop while turn start is pending",
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let task_owner = owner.clone();
        let task_store = store.clone();
        let task_binary = binary.clone();
        let operation = tokio::spawn(async move {
            task_start_with_store_and_binary_fenced(&args, &task_owner, &task_store, &task_binary)
                .await
        });
        wait_for_marker(&turn_started).await;

        // Keep a real lifecycle permit until the task's cancellation path and
        // terminal record update have completed. This makes the shutdown
        // boundary deterministic while the fake child is still in turn/start.
        let drain_gate =
            ensure_current_active_instance(&SessionInstance::from_session(&owner), &owner)
                .await
                .unwrap();
        handle.shutdown().await.unwrap();
        let mut removal = tokio::spawn({
            let owner = owner.clone();
            let store = store.clone();
            async move { remove_session_with_store(&owner, &store).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut removal)
                .await
                .is_err(),
            "session cleanup returned while turn/start cancellation was draining"
        );

        std::fs::write(&release, b"release").unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result["status"], "interrupted");
        let task_id = task_id_for_operation(&owner, operation_id).unwrap();
        assert_eq!(
            store.load(&owner, task_id).unwrap().status,
            TaskStatus::Interrupted
        );
        assert!(runtime_for(&owner, task_id).is_none());

        drop(drain_gate);
        tokio::time::timeout(Duration::from_secs(5), &mut removal)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn restart_does_not_start_replacement_until_old_turn_drained() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("volume");
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let roots =
            crate::named_roots::NamedRoots::from_canonical_roots(std::collections::BTreeMap::from(
                [("src".to_owned(), std::fs::canonicalize(&root).unwrap())],
            ))
            .unwrap();
        let (supervisor, _approvals) = crate::supervisor::SessionSupervisor::new(roots);
        let id = format!("drain-restart-{}", Uuid::new_v4());
        supervisor.start("src/repo", Some(&id)).await.unwrap();
        let old = config::read_session_metadata(&id).await.unwrap();

        let entered = fixture.path().join("restart-turn-start-entered");
        let release = fixture.path().join("restart-turn-start-release");
        let client = blocking_shutdown_client(entered.clone(), release.clone());
        let old_instance = SessionInstance::from_session(&old);
        let operation = tokio::spawn({
            let client = client.clone();
            let old = old.clone();
            let old_instance = old_instance.clone();
            async move {
                request_for_instance(&client, &old_instance, &old, "turn/start", json!({})).await
            }
        });
        wait_for_marker(&entered).await;

        let stopping = {
            let supervisor = Arc::clone(&supervisor);
            let id = id.clone();
            tokio::spawn(async move { supervisor.stop(&id).await })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let lifecycle = config::read_session_lifecycle(&id)
                    .await
                    .unwrap()
                    .expect("old session lifecycle should remain available while stopping");
                let current = config::read_session_metadata(&id).await.unwrap();
                let old_metadata_retained = current.id == old.id
                    && current.started_at == old.started_at
                    && current.process_id == 0;
                if session_instance_is_closing(&old_instance)
                    && lifecycle.started_at == old.started_at
                    && lifecycle.status == config::LifecycleStatus::Stopped
                    && old_metadata_retained
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("old instance did not reach its persisted stopped state while draining");

        let mut starting = {
            let supervisor = Arc::clone(&supervisor);
            let id = id.clone();
            tokio::spawn(async move { supervisor.start("src/repo", Some(&id)).await })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut starting)
                .await
                .is_err(),
            "replacement start completed while the old Codex operation was draining"
        );

        std::fs::write(&release, b"release").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), operation)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(5), stopping)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let replacement = tokio::time::timeout(Duration::from_secs(5), &mut starting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let replacement_session = config::read_session_metadata(&id).await.unwrap();
        assert_eq!(replacement.session_id, id);
        assert_ne!(
            SessionInstance::from_session(&replacement_session),
            old_instance
        );
        assert!(config::session_is_active(&id).await.unwrap());

        supervisor.stop(&id).await.unwrap();
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn drain_timeout_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("drain-timeout-{}", Uuid::new_v4());
        let (handle, owner) = active_test_session(root.path(), &id, true).await;
        let entered = root.path().join("timeout-entered");
        let release = root.path().join("timeout-release");
        let client = blocking_shutdown_client(entered.clone(), release.clone());
        let owner_instance = SessionInstance::from_session(&owner);
        let operation = tokio::spawn({
            let client = client.clone();
            let owner = owner.clone();
            let owner_instance = owner_instance.clone();
            async move {
                request_for_instance(&client, &owner_instance, &owner, "turn/start", json!({}))
                    .await
            }
        });
        wait_for_marker(&entered).await;
        handle.shutdown().await.unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let error =
            remove_session_with_store_with_timeout(&owner, &store, Duration::from_millis(50))
                .await
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("timed out waiting for in-flight")
        );
        assert!(ensure_session_replacement_allowed(&id).is_err());

        let (sender, _receiver) = approvals::approval_channel();
        let replacement = approvals::spawn_runtime(root.path(), Some(&id), false, sender).await;
        assert!(
            replacement.is_err(),
            "replacement started after drain timeout"
        );

        std::fs::write(&release, b"release").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), operation)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        remove_session_with_store(&owner, &store).await.unwrap();
    }

    #[tokio::test]
    async fn registered_runtime_and_inflight_drain_do_not_deadlock() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("drain-registered-{}", Uuid::new_v4());
        let (handle, owner) = active_test_session(root.path(), &id, true).await;
        let entered = root.path().join("registered-entered");
        let release = root.path().join("registered-release");
        let client = blocking_shutdown_client(entered.clone(), release.clone());
        let task_id = Uuid::new_v4();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let runtime_lease = store.try_acquire_runtime_lease(task_id).unwrap().unwrap();
        let owner_instance = SessionInstance::from_session(&owner);
        runtimes().lock().unwrap().insert(
            task_id,
            RuntimeHandle {
                client: client.clone(),
                owner: owner_instance.clone(),
                scope: owner.cwd.clone(),
                instance_id: Uuid::new_v4(),
                started_at: Instant::now(),
                _lease: Arc::new(runtime_lease),
            },
        );
        let operation = tokio::spawn({
            let client = client.clone();
            let owner = owner.clone();
            let owner_instance = owner_instance.clone();
            async move {
                request_for_instance(&client, &owner_instance, &owner, "turn/start", json!({}))
                    .await
            }
        });
        wait_for_marker(&entered).await;
        handle.shutdown().await.unwrap();
        assert!(store.try_acquire_runtime_lease(task_id).unwrap().is_none());
        let removal_store = store.clone();
        let mut removal = tokio::spawn({
            let owner = owner.clone();
            async move { remove_session_with_store(&owner, &removal_store).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut removal)
                .await
                .is_err()
        );

        std::fs::write(&release, b"release").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), operation)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(5), &mut removal)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(runtime_for(&owner, task_id).is_none());
        assert!(store.try_acquire_runtime_lease(task_id).unwrap().is_some());
    }

    #[tokio::test]
    async fn child_approval_permit_drains_before_session_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("drain-approval-{}", Uuid::new_v4());
        let (approval_sender, mut approval_receiver) = approvals::approval_channel();
        let handle = approvals::spawn_runtime(root.path(), Some(&id), false, approval_sender)
            .await
            .unwrap();
        let owner = config::read_session_metadata(&id).await.unwrap();
        let (finished_sender, mut finished_receiver) = oneshot::channel();
        let approval = tokio::spawn({
            let owner = owner.clone();
            async move {
                let allowed = request_child_approval(
                    &owner,
                    "Codex command approval",
                    "approval drain test".to_owned(),
                    BTreeMap::new(),
                )
                .await;
                let _ = finished_sender.send(());
                allowed
            }
        });
        let prompt = tokio::time::timeout(Duration::from_secs(1), approval_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        drop(prompt);

        let store = TaskStore::new(store_root.path().join("tasks"));
        remove_session_with_store(&owner, &store).await.unwrap();
        assert!(
            finished_receiver.try_recv().is_ok(),
            "session cleanup returned while child approval permit was still held"
        );
        assert!(!approval.await.unwrap());

        handle.shutdown().await.unwrap();
        remove_session(&owner).await.unwrap();
    }

    #[tokio::test]
    async fn stop_during_thread_start_never_starts_turn() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("shutdown-thread-{}", Uuid::new_v4());
        let (handle, owner) = active_test_session(root.path(), &id, true).await;
        let store = TaskStore::new(store_root.path().join("tasks"));
        let (binary, entered, release, turn_started, _) =
            barrier_fake_app_server(root.path(), "thread/start");
        let operation_id = Uuid::new_v4();
        let args = json!({
            "operation_id": operation_id,
            "task": "stop while thread start is pending",
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let task_owner = owner.clone();
        let task_store = store.clone();
        let task_binary = binary.clone();
        let operation = tokio::spawn(async move {
            task_start_with_store_and_binary_fenced(&args, &task_owner, &task_store, &task_binary)
                .await
        });

        wait_for_marker(&entered).await;
        handle.shutdown().await.unwrap();
        remove_session_with_store(&owner, &store).await.unwrap();
        std::fs::write(&release, b"release").unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        assert_eq!(result["status"], "interrupted");
        assert!(!turn_started.exists(), "turn/start was sent after shutdown");
        assert!(
            runtime_for(&owner, task_id_for_operation(&owner, operation_id).unwrap()).is_none()
        );
        let record = store
            .load(&owner, task_id_for_operation(&owner, operation_id).unwrap())
            .unwrap();
        assert_eq!(record.status, TaskStatus::Interrupted);
        assert!(record.thread_id.is_none());
        remove_session(&owner).await.unwrap();
    }

    #[tokio::test]
    async fn stop_after_thread_start_before_turn_start() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("shutdown-between-rpcs-{}", Uuid::new_v4());
        let (handle, owner) = active_test_session(root.path(), &id, true).await;
        let methods = Arc::new(Mutex::new(Vec::new()));
        let client = recording_client(Arc::clone(&methods));

        let thread = request_for_instance(
            &client,
            &SessionInstance::from_session(&owner),
            &owner,
            "thread/start",
            json!({}),
        )
        .await
        .unwrap();
        assert_eq!(thread["thread"]["id"], "thread");

        let owner_instance = SessionInstance::from_session(&owner);
        begin_session_instance_shutdown(&owner_instance);
        let turn =
            request_for_instance(&client, &owner_instance, &owner, "turn/start", json!({})).await;
        assert!(turn.is_err());
        assert_eq!(methods.lock().unwrap().as_slice(), ["thread/start"]);

        client.shutdown().await;
        handle.shutdown().await.unwrap();
        remove_session(&owner).await.unwrap();
    }

    #[tokio::test]
    async fn stop_during_task_control_never_sends_steer() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("shutdown-control-{}", Uuid::new_v4());
        let (handle, owner) = active_test_session(root.path(), &id, true).await;
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                task_id,
                TaskStatus::Running,
                1,
                Some("thread"),
                Some("turn"),
            ))
            .unwrap();
        let (binary, entered, release, turn_started, steer_sent) =
            barrier_fake_app_server(root.path(), "thread/resume");
        let operation_id = Uuid::new_v4();
        let args = json!({
            "task_id": task_id,
            "operation_id": operation_id,
            "action": "steer",
            "input": "must not be sent"
        });
        let task_owner = owner.clone();
        let task_store = store.clone();
        let task_binary = binary.clone();
        let operation = tokio::spawn(async move {
            task_control_with_store_and_binary(&args, &task_owner, &task_store, &task_binary).await
        });

        wait_for_marker(&entered).await;
        handle.shutdown().await.unwrap();
        remove_session_with_store(&owner, &store).await.unwrap();
        std::fs::write(&release, b"release").unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        assert_eq!(result["status"], "interrupted");
        assert!(!turn_started.exists(), "turn/start was sent during control");
        assert!(!steer_sent.exists(), "turn/steer was sent after shutdown");
        assert!(runtime_for(&owner, task_id).is_none());
        assert_eq!(
            store.load(&owner, task_id).unwrap().status,
            TaskStatus::Interrupted
        );
        remove_session(&owner).await.unwrap();
    }

    #[tokio::test]
    async fn old_instance_cannot_insert_runtime_after_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("shutdown-insert-{}", Uuid::new_v4());
        let (handle, owner) = active_test_session(root.path(), &id, true).await;
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let methods = Arc::new(Mutex::new(Vec::new()));
        let client = recording_client(methods);

        handle.shutdown().await.unwrap();
        remove_session_with_store(&owner, &store).await.unwrap();
        let runtime_store = TaskStore::new(store_root.path().join("runtime-locks"));
        let lease = runtime_store
            .try_acquire_runtime_lease(task_id)
            .unwrap()
            .unwrap();
        let error = insert_runtime(
            &owner,
            task_id,
            &runtime_store,
            client.clone(),
            Arc::new(lease),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("no longer current")
                || error.to_string().contains("not active")
        );
        assert!(runtime_for(&owner, task_id).is_none());
        client.shutdown().await;
        remove_session(&owner).await.unwrap();
    }

    #[tokio::test]
    async fn old_child_approval_does_not_reach_replacement_session() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("approval-replacement-{}", Uuid::new_v4());
        let (old_handle, old) = active_test_session(root.path(), &id, true).await;
        old_handle.shutdown().await.unwrap();
        remove_session(&old).await.unwrap();

        let (sender, mut receiver) = approvals::approval_channel();
        let replacement_handle = approvals::spawn_runtime(root.path(), Some(&id), false, sender)
            .await
            .unwrap();
        let replacement = config::read_session_metadata(&id).await.unwrap();
        assert_ne!(old.started_at, replacement.started_at);

        let allowed = approvals::request_user_approval_for_instance(
            &old,
            "Codex command approval",
            "old instance prompt".to_owned(),
            old.cwd.clone(),
            BTreeMap::new(),
        )
        .await
        .unwrap();
        assert!(!allowed);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), receiver.recv())
                .await
                .is_err()
        );

        replacement_handle.shutdown().await.unwrap();
        remove_session(&replacement).await.unwrap();
    }

    #[tokio::test]
    async fn replacement_session_still_works() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("replacement-works-{}", Uuid::new_v4());
        let (old_handle, old) = active_test_session(root.path(), &id, true).await;
        old_handle.shutdown().await.unwrap();
        remove_session(&old).await.unwrap();

        let (replacement_handle, replacement) = active_test_session(root.path(), &id, true).await;
        let store = TaskStore::new(store_root.path().join("tasks"));
        let binary = fake_app_server(root.path(), "ok");
        let operation_id = Uuid::new_v4();
        let result = task_start_with_store_and_binary_fenced(
            &json!({
                "operation_id": operation_id,
                "task": "new replacement task",
                "model": "gpt-5.6-luna",
                "effort": "max"
            }),
            &replacement,
            &store,
            &binary,
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "running");
        let task_id = task_id_for_operation(&replacement, operation_id).unwrap();
        assert!(runtime_for(&replacement, task_id).is_some());

        replacement_handle.shutdown().await.unwrap();
        remove_session_with_store(&replacement, &store)
            .await
            .unwrap();
        assert_eq!(
            store.load(&replacement, task_id).unwrap().status,
            TaskStatus::Interrupted
        );
        remove_session(&replacement).await.unwrap();
    }

    #[tokio::test]
    async fn terminal_record_never_resurrected_by_late_response() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let id = format!("late-response-{}", Uuid::new_v4());
        let (handle, owner) = active_test_session(root.path(), &id, true).await;
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                task_id,
                TaskStatus::Accepted,
                1,
                None,
                None,
            ))
            .unwrap();

        handle.shutdown().await.unwrap();
        remove_session_with_store(&owner, &store).await.unwrap();
        let finalized = store.load(&owner, task_id).unwrap();
        let late_thread =
            apply_thread_start_response(&store, &owner, task_id, "late-thread").unwrap();
        let late_turn = apply_turn_start_response(
            &store,
            &owner,
            task_id,
            "late-thread",
            "late-turn",
            Uuid::new_v4(),
        )
        .unwrap();

        for late in [finalized, late_thread, late_turn] {
            assert_eq!(late.status, TaskStatus::Interrupted);
            assert_eq!(late.revision, 2);
            assert!(late.thread_id.is_none());
            assert!(late.turn_id.is_none());
            assert_eq!(late.generation, 0);
        }
        remove_session(&owner).await.unwrap();
    }

    #[tokio::test]
    async fn remove_session_finalizes_running_record_and_preserves_start_idempotency() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "finalize-running", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let operation_id = Uuid::new_v4();
        let task_id = task_id_for_operation(&owner, operation_id).unwrap();
        let args = json!({
            "operation_id": operation_id,
            "task": "finalize on session stop",
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let request_fingerprint = fingerprint(&json!({
            "kind": "start",
            "task_id": task_id,
            "task": "finalize on session stop",
            "model": "gpt-5.6-luna",
            "effort": "max",
        }))
        .unwrap();
        let mut record = task_record(
            &owner,
            task_id,
            TaskStatus::Running,
            2,
            Some("thread"),
            Some("turn"),
        );
        record.operations.push(start_receipt(
            operation_id,
            request_fingerprint,
            OperationPhase::Applied,
            record.outcome(),
        ));
        store.save(&record).unwrap();

        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        let (stopped, stopped_receiver) = oneshot::channel();
        let actor = tokio::spawn(async move {
            if matches!(receiver.recv().await, Some(ClientCommand::Shutdown)) {
                let _ = stopped.send(());
            }
        });
        let runtime_lease = test_runtime_lease(root.path(), task_id);
        runtimes().lock().unwrap().insert(
            task_id,
            RuntimeHandle {
                client: RpcClient {
                    tx: commands,
                    actor: Arc::new(Mutex::new(Some(actor))),

                    connected: Arc::new(AtomicBool::new(true)),
                    pending_approvals: Arc::new(AtomicU64::new(0)),
                    pending_summary_binding: Arc::new(Mutex::new(None)),
                    runtime_instance_id: Arc::new(Mutex::new(None)),
                },
                owner: SessionInstance::from_session(&owner),
                scope: owner.cwd.clone(),
                instance_id: Uuid::new_v4(),
                started_at: Instant::now(),
                _lease: runtime_lease,
            },
        );

        remove_session_with_store(&owner, &store).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), stopped_receiver)
            .await
            .unwrap()
            .unwrap();
        assert!(runtime_for(&owner, task_id).is_none());
        let finalized = store.load(&owner, task_id).unwrap();
        assert_eq!(finalized.status, TaskStatus::Interrupted);
        assert_eq!(finalized.operations[0].phase, OperationPhase::Applied);
        assert_eq!(
            finalized.operations[0].outcome.status,
            TaskStatus::Interrupted
        );

        let replay = task_start_with_store_and_binary(
            &args,
            &owner,
            &store,
            &root.path().join("never-spawned"),
        )
        .await
        .unwrap();
        assert_eq!(replay["status"], "interrupted");
        assert!(runtime_for(&owner, task_id).is_none());
    }

    #[tokio::test]
    async fn remove_session_finalizes_nonterminal_orphan_records_and_accepted_receipts() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "finalize-orphans", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let statuses = [
            TaskStatus::Accepted,
            TaskStatus::WaitingApproval,
            TaskStatus::RetryableFailed,
            TaskStatus::ReconciliationRequired,
            TaskStatus::Unknown,
        ];
        let mut operations = Vec::new();
        for (index, status) in statuses.into_iter().enumerate() {
            let task_id = Uuid::new_v4();
            let operation_id = Uuid::new_v4();
            let request_fingerprint = fingerprint(&json!({"index": index})).unwrap();
            let mut record = task_record(&owner, task_id, status, 1, None, None);
            record.operations.push(start_receipt(
                operation_id,
                request_fingerprint,
                OperationPhase::Accepted,
                record.outcome(),
            ));
            store.save(&record).unwrap();
            operations.push((task_id, operation_id, request_fingerprint));
        }

        remove_session_with_store(&owner, &store).await.unwrap();

        for (task_id, operation_id, request_fingerprint) in operations {
            let finalized = store.load(&owner, task_id).unwrap();
            assert_eq!(finalized.status, TaskStatus::Interrupted);
            assert_eq!(finalized.operations[0].phase, OperationPhase::Applied);
            assert_eq!(
                finalized.operations[0].outcome.status,
                TaskStatus::Interrupted
            );
            let replay = replay_operation(&finalized, operation_id, request_fingerprint).unwrap();
            assert_eq!(replay["status"], "interrupted");
        }
    }

    #[tokio::test]
    async fn finalized_task_expires_and_is_pruned_after_retention() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "finalize-expiry", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                task_id,
                TaskStatus::Running,
                1,
                Some("thread"),
                Some("turn"),
            ))
            .unwrap();

        remove_session_with_store(&owner, &store).await.unwrap();
        let mut expired = store.load(&owner, task_id).unwrap();
        expired.updated_at = config::unix_time().saturating_sub(TASK_RETENTION_SECONDS + 1);
        store.save(&expired).unwrap();
        store
            .save(&task_record(
                &owner,
                Uuid::new_v4(),
                TaskStatus::Accepted,
                1,
                None,
                None,
            ))
            .unwrap();
        assert!(store.load(&owner, task_id).is_err());
    }

    #[tokio::test]
    async fn finalized_task_remains_fenced_from_replacement_session_instance() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "finalize-fence", true);
        let mut replacement = owner.clone();
        replacement.started_at += 1;
        replacement.process_id += 1;
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                task_id,
                TaskStatus::Running,
                1,
                Some("thread"),
                Some("turn"),
            ))
            .unwrap();

        remove_session_with_store(&owner, &store).await.unwrap();
        assert_eq!(
            store.load(&owner, task_id).unwrap().status,
            TaskStatus::Interrupted
        );
        assert!(store.load(&replacement, task_id).is_err());
        assert!(
            store
                .update(&replacement, task_id, |record| {
                    record.status = TaskStatus::Completed;
                    Ok(())
                })
                .is_err()
        );
    }

    #[tokio::test]
    async fn repeated_session_finalization_leaves_only_prunable_terminal_records() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let mut records = Vec::new();
        for index in 0..8 {
            let mut owner = session(root.path(), "repeated-finalize", true);
            owner.started_at += index;
            owner.process_id += index as u32;
            let task_id = Uuid::new_v4();
            store
                .save(&task_record(
                    &owner,
                    task_id,
                    TaskStatus::Running,
                    1,
                    Some("thread"),
                    Some("turn"),
                ))
                .unwrap();
            remove_session_with_store(&owner, &store).await.unwrap();
            assert_eq!(
                store.load(&owner, task_id).unwrap().status,
                TaskStatus::Interrupted
            );
            records.push((owner, task_id));
        }

        let expired_at = config::unix_time().saturating_sub(TASK_RETENTION_SECONDS + 1);
        for (owner, task_id) in &records {
            let mut record = store.load(owner, *task_id).unwrap();
            record.updated_at = expired_at;
            std::fs::write(
                store.path(*task_id),
                serde_json::to_vec_pretty(&record).unwrap(),
            )
            .unwrap();
        }
        let current = session(root.path(), "replacement-finalize", true);
        store
            .save(&task_record(
                &current,
                Uuid::new_v4(),
                TaskStatus::Accepted,
                1,
                None,
                None,
            ))
            .unwrap();
        for (owner, task_id) in records {
            assert!(store.load(&owner, task_id).is_err());
        }
    }

    #[tokio::test]
    async fn managed_session_restart_removes_old_codex_runtime_before_replacement() {
        let root = tempfile::tempdir().unwrap();
        let canonical = config::canonical_directory(root.path()).unwrap();
        let roots = crate::named_roots::NamedRoots::from_canonical_roots(
            std::collections::BTreeMap::from([("src".to_owned(), canonical)]),
        )
        .unwrap();
        let (supervisor, _approval_receiver) = crate::supervisor::SessionSupervisor::new(roots);
        let id = format!("restart-runtime-{}", Uuid::new_v4());
        supervisor
            .start_public_with_environment(
                "src",
                Some(&id),
                approvals::CapturedStartEnvironment::default(),
            )
            .await
            .unwrap();
        let old_session = config::read_session_metadata(&id).await.unwrap();
        let task_id = Uuid::new_v4();
        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        let (stopped, stopped_receiver) = oneshot::channel();
        let actor = tokio::spawn(async move {
            if matches!(receiver.recv().await, Some(ClientCommand::Shutdown)) {
                let _ = stopped.send(());
            }
        });
        let runtime_lease = test_runtime_lease(root.path(), task_id);
        runtimes().lock().unwrap().insert(
            task_id,
            RuntimeHandle {
                client: RpcClient {
                    tx: commands,
                    actor: Arc::new(Mutex::new(Some(actor))),

                    connected: Arc::new(AtomicBool::new(true)),
                    pending_approvals: Arc::new(AtomicU64::new(0)),
                    pending_summary_binding: Arc::new(Mutex::new(None)),
                    runtime_instance_id: Arc::new(Mutex::new(None)),
                },
                owner: SessionInstance::from_session(&old_session),
                scope: old_session.cwd.clone(),
                instance_id: Uuid::new_v4(),
                started_at: Instant::now(),
                _lease: runtime_lease,
            },
        );

        let backend = crate::session_control::SessionBackend::in_process(Arc::clone(&supervisor));
        backend.restart(&id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), stopped_receiver)
            .await
            .unwrap()
            .unwrap();
        assert!(runtime_for(&old_session, task_id).is_none());
        assert!(config::session_is_active(&id).await.unwrap());

        supervisor.shutdown().await.unwrap();
        let _ = tokio::fs::remove_file(config::socket_path(&id).unwrap()).await;
        let _ = tokio::fs::remove_file(config::session_path(&id).unwrap()).await;
        let _ = tokio::fs::remove_file(config::session_lifecycle_path(&id).unwrap()).await;
    }

    #[tokio::test]
    async fn yolo_task_keeps_codex_approval_policy_independent() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "yolo-codex-policy", true);
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let operation_id = Uuid::new_v4();
        let args = json!({
            "operation_id": operation_id,
            "task": "run the bounded test",
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let result = task_start_with_store_and_binary(
            &args,
            &session,
            &store,
            &fake_app_server(root.path(), "reject-never"),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "running");
        remove_session(&session).await.unwrap();
    }

    #[test]
    fn concurrent_start_acceptance_has_one_durable_winner() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "concurrent-start", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let operation_id = Uuid::new_v4();
        let task_id = task_id_for_operation(&owner, operation_id).unwrap();
        let request_fingerprint = fingerprint(&json!({"task":"same"})).unwrap();
        let now = config::unix_time();
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(&owner),
            scope_cwd: owner.cwd.clone(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status: TaskStatus::Accepted,
            revision: 1,
            generation: 0,
            thread_id: None,
            continued_from_task_id: None,
            continued_by_task_id: None,
            continued_by_request_fingerprint: None,
            turn_id: None,
            previous_turn_id: None,
            usage: None,
            report: None,
            report_source: None,
            report_status: None,
            pending_interaction: None,
            created_at: now,
            updated_at: now,
            verification: None,
            delivery: None,
            operations: vec![OperationReceipt {
                operation_id,
                request_fingerprint,
                action: "start".to_owned(),
                phase: OperationPhase::Accepted,
                outcome: OperationOutcome {
                    status: TaskStatus::Accepted,
                    revision: 1,
                    generation: 0,
                    thread_id: None,
                    turn_id: None,
                },
            }],
            operation_tombstones: Vec::new(),
        };
        let barrier = Arc::new(Barrier::new(3));
        let (sender, receiver) = mpsc::channel();

        std::thread::scope(|scope| {
            for candidate in [record.clone(), record] {
                let store = store.clone();
                let owner = owner.clone();
                let barrier = Arc::clone(&barrier);
                let sender = sender.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let accepted = matches!(
                        store.accept_start(&owner, candidate).unwrap(),
                        StartAcceptance::Accepted(..)
                    );
                    sender.send(accepted).unwrap();
                });
            }
            barrier.wait();
        });
        drop(sender);

        let results: Vec<_> = receiver.iter().collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results.iter().filter(|accepted| **accepted).count(), 1);
        let persisted = store.load(&owner, task_id).unwrap();
        assert_eq!(persisted.operations.len(), 1);
    }

    #[tokio::test]
    async fn cross_process_store_and_runtime_ownership_are_fenced() {
        const TEST_NAME: &str =
            "codex_app_server::tests::cross_process_store_and_runtime_ownership_are_fenced";
        const ROLE: &str = "TEMOTE_TEST_CODEX_CROSS_PROCESS_ROLE";
        const STORE: &str = "TEMOTE_TEST_CODEX_CROSS_PROCESS_STORE";
        const WORKSPACE: &str = "TEMOTE_TEST_CODEX_CROSS_PROCESS_WORKSPACE";
        const TASK_ID: &str = "TEMOTE_TEST_CODEX_CROSS_PROCESS_TASK_ID";
        const READY: &str = "TEMOTE_TEST_CODEX_CROSS_PROCESS_READY";
        const RELEASE: &str = "TEMOTE_TEST_CODEX_CROSS_PROCESS_RELEASE";
        const STOPPED: &str = "TEMOTE_TEST_CODEX_CROSS_PROCESS_STOPPED";
        const OWNER: &str = "TEMOTE_TEST_CODEX_CROSS_PROCESS_OWNER";

        if let Some(role) = std::env::var_os(ROLE) {
            let store = TaskStore::new(PathBuf::from(std::env::var_os(STORE).unwrap()));
            let workspace = PathBuf::from(std::env::var_os(WORKSPACE).unwrap());
            let task_id = Uuid::parse_str(&std::env::var(TASK_ID).unwrap()).unwrap();
            let ready = PathBuf::from(std::env::var_os(READY).unwrap());
            let release = PathBuf::from(std::env::var_os(RELEASE).unwrap());
            let stopped = PathBuf::from(std::env::var_os(STOPPED).unwrap());
            let owner_path = PathBuf::from(std::env::var_os(OWNER).unwrap());
            match role.to_str().unwrap() {
                "holder" => {
                    let _lease = store
                        .try_acquire_runtime_lease(task_id)
                        .unwrap()
                        .expect("child could not acquire task runtime lease");
                    std::fs::write(&ready, b"ready").unwrap();
                    wait_for_child_release(&release).await;
                }
                "updater" => {
                    std::fs::write(&ready, b"ready").unwrap();
                    wait_for_child_release(&release).await;
                    let owner = session(&workspace, "cross-process-owner", true);
                    store
                        .update(&owner, task_id, |record| {
                            std::thread::sleep(Duration::from_millis(75));
                            record.revision = record.revision.saturating_add(1);
                            Ok(())
                        })
                        .unwrap();
                }
                "runtime-holder" => {
                    let (session_handle, owner) =
                        active_test_session(&workspace, "cross-process-live-owner", true).await;
                    let socket_path = config::socket_path(&owner.id).unwrap();
                    std::fs::write(
                        &owner_path,
                        serde_json::to_vec(&(owner.clone(), socket_path)).unwrap(),
                    )
                    .unwrap();
                    store
                        .save(&task_record(
                            &owner,
                            task_id,
                            TaskStatus::Running,
                            20,
                            Some("thread"),
                            Some("turn"),
                        ))
                        .unwrap();
                    let lease = Arc::new(
                        store
                            .try_acquire_runtime_lease(task_id)
                            .unwrap()
                            .expect("child could not acquire watched task runtime lease"),
                    );
                    let (commands, mut receiver) = tokio::sync::mpsc::channel(4);
                    let actor = tokio::spawn(async move {
                        let mut pending = Vec::new();
                        while let Some(command) = receiver.recv().await {
                            match command {
                                ClientCommand::Request { reply, .. } => {
                                    pending.push(reply);
                                    std::fs::write(&ready, b"ready").unwrap();
                                }
                                ClientCommand::Shutdown => {
                                    std::fs::write(&stopped, b"stopped").unwrap();
                                    break;
                                }
                                ClientCommand::Notify { .. } => {}
                            }
                        }
                    });
                    let client = RpcClient {
                        tx: commands,
                        actor: Arc::new(Mutex::new(Some(actor))),

                        connected: Arc::new(AtomicBool::new(true)),
                        pending_approvals: Arc::new(AtomicU64::new(0)),
                        pending_summary_binding: Arc::new(Mutex::new(None)),
                        runtime_instance_id: Arc::new(Mutex::new(None)),
                    };
                    insert_runtime(&owner, task_id, &store, client.clone(), lease)
                        .await
                        .unwrap();
                    let request_client = client.clone();
                    let request_owner = SessionInstance::from_session(&owner);
                    let request_session = owner.clone();
                    let request = tokio::spawn(async move {
                        request_for_instance(
                            &request_client,
                            &request_owner,
                            &request_session,
                            "thread/read",
                            json!({"threadId":"thread","includeTurns":true}),
                        )
                        .await
                    });
                    wait_for_child_release(&release).await;
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            if ensure_session_replacement_allowed(&owner.id).is_ok() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .expect("runtime owner lifecycle remained stuck after request drain");
                    assert!(request.await.unwrap().is_err());
                    session_handle.shutdown().await.unwrap();
                }
                other => panic!("unknown cross-process test role {other}"),
            }
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(root.path().join("tasks"));
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let owner = session(&workspace, "cross-process-owner", true);
        let task_id = Uuid::new_v4();
        let record = task_record(
            &owner,
            task_id,
            TaskStatus::Running,
            10,
            Some("thread"),
            Some("turn"),
        );
        store.save(&record).unwrap();

        let current_exe = std::env::current_exe().unwrap();
        let spawn_child = |role: &str, child_task_id: Uuid, ready: &Path, release: &Path| {
            let stopped = ready.with_extension("stopped");
            let owner = ready.with_extension("owner.json");
            ChildGuard::new(
                std::process::Command::new(&current_exe)
                    .args(["--exact", TEST_NAME, "--nocapture"])
                    .env(ROLE, role)
                    .env(STORE, &store.directory)
                    .env(WORKSPACE, &workspace)
                    .env(TASK_ID, child_task_id.to_string())
                    .env(READY, ready)
                    .env(RELEASE, release)
                    .env(STOPPED, stopped)
                    .env(OWNER, owner)
                    .spawn()
                    .unwrap(),
            )
        };

        let holder_ready = root.path().join("holder-ready");
        let holder_release = root.path().join("holder-release");
        let mut holder = spawn_child("holder", task_id, &holder_ready, &holder_release);
        wait_for_marker(&holder_ready).await;
        assert!(store.try_acquire_runtime_lease(task_id).unwrap().is_none());

        let cached = task_view_at_revision(&record, None);
        assert_eq!(cached["status"], "running");
        assert_eq!(cached["revision"], 10);
        assert_eq!(cached["last_updated_at"], record.updated_at);
        assert_eq!(cached["reconciliation_deferred"], true);
        let not_modified = task_view_at_revision(&record, Some(10));
        assert_eq!(not_modified["status"], "not_modified");
        assert_eq!(not_modified["reconciliation_deferred"], true);

        let control_error = store
            .accept_control(
                &owner,
                task_id,
                Uuid::new_v4(),
                fingerprint(&json!({"operation":"remote-control"})).unwrap(),
                "interrupt",
            )
            .unwrap_err();
        assert!(
            control_error
                .to_string()
                .contains("CODEX_TASK_RUNTIME_OWNED")
        );
        assert!(store.load(&owner, task_id).unwrap().operations.is_empty());

        let expired_task_id = Uuid::new_v4();
        let mut expired = task_record(
            &owner,
            expired_task_id,
            TaskStatus::Completed,
            1,
            Some("expired-thread"),
            Some("expired-turn"),
        );
        expired.updated_at = 0;
        store.save(&expired).unwrap();
        let expired_ready = root.path().join("expired-ready");
        let expired_release = root.path().join("expired-release");
        let mut expired_holder =
            spawn_child("holder", expired_task_id, &expired_ready, &expired_release);
        wait_for_marker(&expired_ready).await;
        store.save(&record).unwrap();
        assert!(store.path(expired_task_id).exists());
        std::fs::write(&expired_release, b"release").unwrap();
        assert!(expired_holder.wait().unwrap().success());
        store.save(&record).unwrap();
        assert!(!store.path(expired_task_id).exists());
        assert!(!store.runtime_lock_path(expired_task_id).exists());

        let update_release = root.path().join("update-release");
        let mut updaters = Vec::new();
        let mut updater_ready = Vec::new();
        for index in 0..4 {
            let ready = root.path().join(format!("updater-{index}-ready"));
            updaters.push(spawn_child("updater", task_id, &ready, &update_release));
            updater_ready.push(ready);
        }
        for ready in &updater_ready {
            wait_for_marker(ready).await;
        }
        std::fs::write(&update_release, b"release").unwrap();
        for updater in &mut updaters {
            assert!(updater.wait().unwrap().success());
        }
        assert_eq!(store.load(&owner, task_id).unwrap().revision, 14);
        let deferred = store.finalize_owner(&record.owner).unwrap();
        assert_eq!(deferred.finalized, 0);
        assert!(deferred.deferred);
        let remotely_running = store.load(&owner, task_id).unwrap();
        assert_eq!(remotely_running.status, TaskStatus::Running);
        assert_eq!(remotely_running.revision, 14);

        std::fs::write(&holder_release, b"release").unwrap();
        assert!(holder.wait().unwrap().success());
        let completed = store.finalize_owner(&record.owner).unwrap();
        assert_eq!(completed.finalized, 1);
        assert!(!completed.deferred);
        let finalized = store.load(&owner, task_id).unwrap();
        assert_eq!(finalized.status, TaskStatus::Interrupted);
        assert_eq!(finalized.revision, 15);
        let released = store
            .try_acquire_runtime_lease(task_id)
            .unwrap()
            .expect("runtime lease stayed locked after owner exit");
        #[cfg(unix)]
        {
            // SAFETY: the lease owns this valid descriptor.
            let flags = unsafe { libc::fcntl(released.file.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        drop(released);

        let terminal_task_id = Uuid::new_v4();
        let terminal_session_id = format!("cross-process-terminal-{}", Uuid::new_v4());
        let (terminal_handle, terminal_owner) =
            active_test_session(&workspace, &terminal_session_id, true).await;
        let terminal_record = task_record(
            &terminal_owner,
            terminal_task_id,
            TaskStatus::Completed,
            30,
            Some("terminal-thread"),
            Some("terminal-turn"),
        );
        store.save(&terminal_record).unwrap();
        let terminal_ready = root.path().join("terminal-ready");
        let terminal_release = root.path().join("terminal-release");
        let mut terminal_holder = spawn_child(
            "holder",
            terminal_task_id,
            &terminal_ready,
            &terminal_release,
        );
        wait_for_marker(&terminal_ready).await;
        terminal_handle.shutdown().await.unwrap();
        let mut terminal_cleanup = tokio::spawn({
            let store = store.clone();
            let terminal_owner = terminal_owner.clone();
            async move { remove_session_with_store(&terminal_owner, &store).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(75), &mut terminal_cleanup)
                .await
                .is_err(),
            "session cleanup completed while a terminal task runtime remained remotely leased"
        );
        assert!(ensure_session_replacement_allowed(&terminal_owner.id).is_err());
        std::fs::write(&terminal_release, b"release").unwrap();
        assert!(terminal_holder.wait().unwrap().success());
        tokio::time::timeout(Duration::from_secs(5), terminal_cleanup)
            .await
            .expect("terminal task cleanup did not resume after runtime lease release")
            .unwrap()
            .unwrap();
        assert!(ensure_session_replacement_allowed(&terminal_owner.id).is_ok());
        let preserved_terminal = store.load(&terminal_owner, terminal_task_id).unwrap();
        assert_eq!(preserved_terminal.status, TaskStatus::Completed);
        assert_eq!(preserved_terminal.revision, 30);

        let watched_task_id = Uuid::new_v4();
        let watched_ready = root.path().join("watched-ready");
        let watched_stopped = watched_ready.with_extension("stopped");
        let watched_release = root.path().join("watched-release");
        let mut watched = spawn_child(
            "runtime-holder",
            watched_task_id,
            &watched_ready,
            &watched_release,
        );
        wait_for_marker(&watched_ready).await;
        assert!(
            store
                .try_acquire_runtime_lease(watched_task_id)
                .unwrap()
                .is_none()
        );
        let watched_owner_path = watched_ready.with_extension("owner.json");
        let (watched_owner, watched_socket): (config::Session, PathBuf) =
            serde_json::from_slice(&std::fs::read(watched_owner_path).unwrap()).unwrap();
        assert_eq!(watched_owner.id, "cross-process-live-owner");
        assert_eq!(
            watched_owner.cwd,
            std::fs::canonicalize(&workspace).unwrap()
        );
        std::fs::remove_file(watched_socket).unwrap();
        wait_for_marker(&watched_stopped).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let persisted = store.load(&watched_owner, watched_task_id).unwrap();
                if persisted.status == TaskStatus::Interrupted {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("runtime owner did not finalize task after external session stop");
        assert!(
            store
                .try_acquire_runtime_lease(watched_task_id)
                .unwrap()
                .is_some()
        );
        std::fs::write(&watched_release, b"release").unwrap();
        assert!(watched.wait().unwrap().success());
    }

    #[test]
    fn concurrent_control_acceptance_is_idempotent_for_duplicate_operation() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "concurrent-control", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let request_fingerprint = fingerprint(&json!({"action":"interrupt"})).unwrap();
        let now = config::unix_time();
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(&owner),
            scope_cwd: owner.cwd.clone(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status: TaskStatus::Running,
            revision: 1,
            generation: 1,
            thread_id: Some("thread-1".to_owned()),
            continued_from_task_id: None,
            continued_by_task_id: None,
            continued_by_request_fingerprint: None,
            turn_id: Some("turn-1".to_owned()),
            previous_turn_id: None,
            usage: None,
            report: None,
            report_source: None,
            report_status: None,
            pending_interaction: None,
            created_at: now,
            updated_at: now,
            verification: None,
            delivery: None,
            operations: Vec::new(),
            operation_tombstones: Vec::new(),
        };
        store.save(&record).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let (sender, receiver) = mpsc::channel();

        std::thread::scope(|scope| {
            for _ in 0..2 {
                let store = store.clone();
                let owner = owner.clone();
                let barrier = Arc::clone(&barrier);
                let sender = sender.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let accepted = matches!(
                        store
                            .accept_control(
                                &owner,
                                task_id,
                                operation_id,
                                request_fingerprint,
                                "interrupt",
                            )
                            .unwrap(),
                        ControlAcceptance::Accepted(..)
                    );
                    sender.send(accepted).unwrap();
                });
            }
            barrier.wait();
        });
        drop(sender);

        let results: Vec<_> = receiver.iter().collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results.iter().filter(|accepted| **accepted).count(), 1);
        let persisted = store.load(&owner, task_id).unwrap();
        assert_eq!(persisted.operations.len(), 1);
        assert_eq!(persisted.operations[0].operation_id, operation_id);
    }

    #[test]
    fn concurrent_completion_and_control_acceptance_never_resurrects_task() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "completion-control", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let request_fingerprint = fingerprint(&json!({"action":"interrupt"})).unwrap();
        let now = config::unix_time();
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(&owner),
            scope_cwd: owner.cwd.clone(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status: TaskStatus::Running,
            revision: 1,
            generation: 1,
            thread_id: Some("thread-1".to_owned()),
            continued_from_task_id: None,
            continued_by_task_id: None,
            continued_by_request_fingerprint: None,
            turn_id: Some("turn-1".to_owned()),
            previous_turn_id: None,
            usage: None,
            report: None,
            report_source: None,
            report_status: None,
            pending_interaction: None,
            created_at: now,
            updated_at: now,
            verification: None,
            delivery: None,
            operations: Vec::new(),
            operation_tombstones: Vec::new(),
        };
        store.save(&record).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let (sender, receiver) = mpsc::channel();

        std::thread::scope(|scope| {
            let completion_store = store.clone();
            let completion_owner = owner.clone();
            let completion_barrier = Arc::clone(&barrier);
            let completion_sender = sender.clone();
            scope.spawn(move || {
                completion_barrier.wait();
                let completed = completion_store
                    .update(&completion_owner, task_id, |record| {
                        record.status = TaskStatus::Completed;
                        record.revision = record.revision.saturating_add(1);
                        Ok(())
                    })
                    .is_ok();
                completion_sender.send(("completion", completed)).unwrap();
            });

            let control_store = store.clone();
            let control_owner = owner.clone();
            let control_barrier = Arc::clone(&barrier);
            let control_sender = sender.clone();
            scope.spawn(move || {
                control_barrier.wait();
                let accepted = match control_store.accept_control(
                    &control_owner,
                    task_id,
                    operation_id,
                    request_fingerprint,
                    "interrupt",
                ) {
                    Ok(ControlAcceptance::Accepted(..)) => true,
                    Ok(ControlAcceptance::Replay(_)) | Err(_) => false,
                };
                control_sender.send(("control", accepted)).unwrap();
            });
            barrier.wait();
        });
        drop(sender);

        let outcomes: Vec<_> = receiver.iter().collect();
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes
                .iter()
                .any(|(operation, success)| *operation == "completion" && *success)
        );
        let control_accepted = outcomes
            .iter()
            .find(|(operation, _)| *operation == "control")
            .map(|(_, accepted)| *accepted)
            .unwrap();
        let persisted = store.load(&owner, task_id).unwrap();
        assert_eq!(persisted.status, TaskStatus::Completed);
        assert_eq!(persisted.operations.len(), usize::from(control_accepted));
    }

    #[test]
    fn control_acceptance_rejects_terminal_tasks_without_recording_operation() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "terminal-control", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let record = TaskRecord {
            schema_version: TASK_SCHEMA_VERSION,
            task_id,
            owner: SessionInstance::from_session(&owner),
            scope_cwd: owner.cwd.clone(),
            model: "gpt-5.6-luna".to_owned(),
            effort: "max".to_owned(),
            status: TaskStatus::Completed,
            revision: 3,
            generation: 1,
            thread_id: Some("thread-1".to_owned()),
            continued_from_task_id: None,
            continued_by_task_id: None,
            continued_by_request_fingerprint: None,
            turn_id: Some("turn-1".to_owned()),
            previous_turn_id: None,
            usage: None,
            report: None,
            report_source: None,
            report_status: None,
            pending_interaction: None,
            created_at: config::unix_time(),
            updated_at: config::unix_time(),
            verification: None,
            delivery: None,
            operations: Vec::new(),
            operation_tombstones: Vec::new(),
        };
        store.save(&record).unwrap();

        let error = match store.accept_control(
            &owner,
            task_id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "interrupt",
        ) {
            Ok(_) => panic!("terminal task unexpectedly accepted control"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("not active"));
        assert!(store.load(&owner, task_id).unwrap().operations.is_empty());
    }

    #[test]
    fn terminal_steer_retains_task_and_durably_claims_fresh_turn_generation() {
        for status in [TaskStatus::Completed, TaskStatus::Interrupted] {
            let root = tempfile::tempdir().unwrap();
            let store = TaskStore::new(root.path().join("tasks"));
            let owner = session(root.path(), "terminal-steer", true);
            let task_id = Uuid::new_v4();
            let mut record =
                task_record(&owner, task_id, status, 3, Some("thread-1"), Some("turn-1"));
            record.generation = 2;
            store.save(&record).unwrap();
            let operation_id = Uuid::new_v4();
            let request_fingerprint =
                fingerprint(&json!({"action":"steer","input":"next"})).unwrap();
            let accepted = store
                .accept_control(&owner, task_id, operation_id, request_fingerprint, "steer")
                .unwrap();
            assert!(matches!(accepted, ControlAcceptance::Accepted(..)));
            let retained = store.load(&owner, task_id).unwrap();
            assert_eq!(retained.task_id, task_id);
            assert_eq!(retained.status, TaskStatus::ReconciliationRequired);
            assert_eq!(retained.generation, 3);
            assert_eq!(retained.previous_turn_id.as_deref(), Some("turn-1"));
            assert!(retained.turn_id.is_none());
            assert_eq!(retained.operations[0].operation_id, operation_id);
            assert!(matches!(
                store
                    .accept_control(&owner, task_id, operation_id, request_fingerprint, "steer")
                    .unwrap(),
                ControlAcceptance::Replay(_)
            ));
        }
    }

    #[test]
    fn successor_reconciliation_does_not_select_a_turn_before_predecessor() {
        let response = json!({"thread":{"status":{"type":"idle"},"turns":[
            {"id":"older","status":"completed"},
            {"id":"predecessor","status":"completed"}
        ]}});
        let state = derive_thread_state_after(&response, None, Some("predecessor")).unwrap();
        assert_eq!(state.status, TaskStatus::ReconciliationRequired);
        assert!(state.turn_id.is_none());
    }

    #[test]
    fn task_store_refuses_capacity_when_all_scoped_tasks_are_active() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "active-capacity", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let mut ids = Vec::new();
        for _ in 0..MAX_TASKS_PER_SCOPE {
            let task_id = Uuid::new_v4();
            ids.push(task_id);
            store
                .save(&task_record(
                    &owner,
                    task_id,
                    TaskStatus::Running,
                    1,
                    Some("thread"),
                    Some("turn"),
                ))
                .unwrap();
        }
        let candidate = task_record(&owner, Uuid::new_v4(), TaskStatus::Accepted, 1, None, None);
        let error = store.save(&candidate).unwrap_err();
        assert!(error.to_string().contains("retention limit"));
        for task_id in ids {
            assert_eq!(
                store.load(&owner, task_id).unwrap().status,
                TaskStatus::Running
            );
        }
        let persisted_count = std::fs::read_dir(&store.directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            })
            .count();
        assert_eq!(persisted_count, MAX_TASKS_PER_SCOPE);
    }

    #[test]
    fn task_store_retains_fresh_terminal_and_prunes_expired_records() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "terminal-capacity", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let terminal_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                terminal_id,
                TaskStatus::Completed,
                1,
                Some("thread"),
                Some("turn"),
            ))
            .unwrap();
        for _ in 0..MAX_TASKS_PER_SCOPE - 1 {
            store
                .save(&task_record(
                    &owner,
                    Uuid::new_v4(),
                    TaskStatus::Running,
                    1,
                    Some("thread"),
                    Some("turn"),
                ))
                .unwrap();
        }
        let candidate_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                candidate_id,
                TaskStatus::Running,
                1,
                Some("thread"),
                Some("turn"),
            ))
            .unwrap_err();
        assert!(store.load(&owner, terminal_id).is_ok());
        assert!(store.load(&owner, candidate_id).is_err());

        let expired_store_root = tempfile::tempdir().unwrap();
        let expired_store = TaskStore::new(expired_store_root.path().join("tasks"));
        let expired_id = Uuid::new_v4();
        let mut expired = task_record(
            &owner,
            expired_id,
            TaskStatus::Completed,
            1,
            Some("thread"),
            Some("turn"),
        );
        expired.updated_at = config::unix_time().saturating_sub(TASK_RETENTION_SECONDS + 1);
        expired_store.save(&expired).unwrap();
        let fresh_id = Uuid::new_v4();
        expired_store
            .save(&task_record(
                &owner,
                fresh_id,
                TaskStatus::Running,
                1,
                Some("thread"),
                Some("turn"),
            ))
            .unwrap();
        assert!(expired_store.load(&owner, expired_id).is_err());
    }

    #[tokio::test]
    async fn retained_terminal_start_replays_after_capacity_pressure() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "retained-start", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let operation_id = Uuid::new_v4();
        let task_id = task_id_for_operation(&owner, operation_id).unwrap();
        let args = json!({
            "operation_id": operation_id,
            "task": "replay retained task",
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let request_fingerprint = fingerprint(&json!({
            "kind": "start",
            "task_id": task_id,
            "task": "replay retained task",
            "model": "gpt-5.6-luna",
            "effort": "max",
        }))
        .unwrap();
        let mut original = task_record(
            &owner,
            task_id,
            TaskStatus::Completed,
            2,
            Some("thread"),
            Some("turn"),
        );
        original.operations.push(start_receipt(
            operation_id,
            request_fingerprint,
            OperationPhase::Applied,
            original.outcome(),
        ));
        store.save(&original).unwrap();
        for _ in 0..MAX_TASKS_PER_SCOPE - 1 {
            store
                .save(&task_record(
                    &owner,
                    Uuid::new_v4(),
                    TaskStatus::Running,
                    1,
                    Some("thread"),
                    Some("turn"),
                ))
                .unwrap();
        }

        let missing_binary = root.path().join("never-spawned");
        for _ in 0..8 {
            let new_args = json!({
                "operation_id": Uuid::new_v4(),
                "task": "must be rejected at capacity",
                "model": "gpt-5.6-luna",
                "effort": "max"
            });
            let capacity_error =
                task_start_with_store_and_binary(&new_args, &owner, &store, &missing_binary)
                    .await
                    .unwrap_err();
            assert!(capacity_error.to_string().contains("retention limit"));
        }
        assert_eq!(
            std::fs::read_dir(store.runtime_lock_directory())
                .unwrap()
                .count(),
            0
        );

        let replay = task_start_with_store_and_binary(&args, &owner, &store, &missing_binary)
            .await
            .unwrap();
        assert_eq!(replay["task_id"], task_id.to_string());
        assert_eq!(replay["status"], "completed");
        let persisted = store.load(&owner, task_id).unwrap();
        assert_eq!(persisted.status, TaskStatus::Completed);
        assert_eq!(persisted.operations[0].phase, OperationPhase::Applied);
    }

    #[tokio::test]
    async fn task_store_does_not_prune_terminal_runtime_backed_records() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "runtime-backed-prune", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let runtime_task_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                runtime_task_id,
                TaskStatus::Completed,
                1,
                Some("thread"),
                Some("turn"),
            ))
            .unwrap();

        let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
        let (stopped, stopped_receiver) = oneshot::channel();
        let actor = tokio::spawn(async move {
            if matches!(receiver.recv().await, Some(ClientCommand::Shutdown)) {
                let _ = stopped.send(());
            }
        });
        let runtime_lease = test_runtime_lease(root.path(), runtime_task_id);
        runtimes().lock().unwrap().insert(
            runtime_task_id,
            RuntimeHandle {
                client: RpcClient {
                    tx: commands,
                    actor: Arc::new(Mutex::new(Some(actor))),

                    connected: Arc::new(AtomicBool::new(true)),
                    pending_approvals: Arc::new(AtomicU64::new(0)),
                    pending_summary_binding: Arc::new(Mutex::new(None)),
                    runtime_instance_id: Arc::new(Mutex::new(None)),
                },
                owner: SessionInstance::from_session(&owner),
                scope: owner.cwd.clone(),
                instance_id: Uuid::new_v4(),
                started_at: Instant::now(),
                _lease: runtime_lease,
            },
        );

        for _ in 0..MAX_TASKS_PER_SCOPE - 1 {
            store
                .save(&task_record(
                    &owner,
                    Uuid::new_v4(),
                    TaskStatus::Running,
                    1,
                    Some("thread"),
                    Some("turn"),
                ))
                .unwrap();
        }
        let candidate = task_record(&owner, Uuid::new_v4(), TaskStatus::Running, 1, None, None);
        let error = store.save(&candidate).unwrap_err();
        assert!(error.to_string().contains("retention limit"));
        assert_eq!(
            store.load(&owner, runtime_task_id).unwrap().status,
            TaskStatus::Completed
        );
        assert!(runtime_for(&owner, runtime_task_id).is_some());

        remove_session(&owner).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), stopped_receiver)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn compacted_operation_receipts_remain_idempotent_for_task_retention() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "operation-retention", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let mut record = task_record(
            &owner,
            task_id,
            TaskStatus::Running,
            1,
            Some("thread"),
            Some("turn"),
        );
        let start_id = Uuid::new_v4();
        let start_fingerprint = fingerprint(&json!({"kind":"start"})).unwrap();
        record.operations.push(start_receipt(
            start_id,
            start_fingerprint,
            OperationPhase::Applied,
            record.outcome(),
        ));
        store.save(&record).unwrap();

        let mut first_control = None;
        for index in 0..(MAX_OPERATION_HISTORY + 8) {
            let operation_id = Uuid::new_v4();
            let request_fingerprint = fingerprint(&json!({"index":index})).unwrap();
            if first_control.is_none() {
                first_control = Some((operation_id, request_fingerprint));
            }
            let accepted = store
                .accept_control(
                    &owner,
                    task_id,
                    operation_id,
                    request_fingerprint,
                    "interrupt",
                )
                .unwrap();
            assert!(matches!(accepted, ControlAcceptance::Accepted(..)));
        }
        let persisted = store.load(&owner, task_id).unwrap();
        let (old_operation_id, old_fingerprint) = first_control.unwrap();
        assert!(
            persisted
                .operation_tombstones
                .iter()
                .any(|tombstone| tombstone.operation_id == old_operation_id)
        );
        let exact_error = store
            .accept_control(
                &owner,
                task_id,
                old_operation_id,
                old_fingerprint,
                "interrupt",
            )
            .unwrap_err();
        assert!(
            exact_error
                .to_string()
                .contains("OPERATION_REPLAY_COMPACTED")
        );
        let conflict = store
            .accept_control(
                &owner,
                task_id,
                old_operation_id,
                Uuid::new_v4(),
                "interrupt",
            )
            .unwrap_err();
        assert!(conflict.to_string().contains("OPERATION_CONFLICT"));
        assert_eq!(
            store
                .load(&owner, task_id)
                .unwrap()
                .operation_tombstones
                .len(),
            persisted.operation_tombstones.len()
        );
    }

    #[test]
    fn reconciliation_snapshot_preserves_retention_and_fences_newer_state() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "codex-snapshot-fence", false);
        let owner = SessionInstance::from_session(&session);
        codex_lifecycle_registry()
            .lock()
            .unwrap()
            .entries
            .insert(owner.clone(), CodexLifecycleEntry::new());
        let store = TaskStore::new(root.path().join("tasks"));
        let mut expected = task_record(
            &session,
            Uuid::new_v4(),
            TaskStatus::Running,
            7,
            Some("thread"),
            Some("turn"),
        );
        expected.updated_at = expected.updated_at.saturating_sub(60);
        store.save(&expected).unwrap();
        for _ in 0..2 {
            let result =
                reconcile_snapshot(&session, &owner, &store, &expected, |_| Ok(())).unwrap();
            assert!(!result.deferred);
            assert_eq!(result.record, expected);
        }
        for changed_field in ["revision", "generation", "thread", "turn"] {
            let mut newer = expected.clone();
            match changed_field {
                "revision" => newer.revision += 1,
                "generation" => newer.generation += 1,
                "thread" => newer.thread_id = Some("new-thread".to_owned()),
                "turn" => newer.turn_id = Some("new-turn".to_owned()),
                _ => unreachable!(),
            }
            store.save(&newer).unwrap();
            // The same fence surrounds successful observations, malformed
            // reads and transport failures: none may revise a newer task.
            for stale_status in [
                TaskStatus::Completed,
                TaskStatus::Unknown,
                TaskStatus::ReconciliationRequired,
            ] {
                let result = reconcile_snapshot(&session, &owner, &store, &expected, |record| {
                    record.status = stale_status;
                    record.revision += 1;
                    Ok(())
                })
                .unwrap();
                assert!(result.deferred);
                assert_eq!(result.record, newer);
                assert_eq!(store.load(&session, expected.task_id).unwrap(), newer);
            }
        }
        codex_lifecycle_registry()
            .lock()
            .unwrap()
            .entries
            .remove(&owner);
    }

    #[test]
    fn in_flight_control_guard_is_scoped_counted_and_not_a_retained_receipt() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "codex-control-guard", false);
        let owner = SessionInstance::from_session(&session);
        codex_lifecycle_registry()
            .lock()
            .unwrap()
            .entries
            .insert(owner.clone(), CodexLifecycleEntry::new());
        let mut record = task_record(
            &session,
            Uuid::new_v4(),
            TaskStatus::Running,
            7,
            Some("thread"),
            Some("turn"),
        );
        let first = TaskControlGuard::acquire(&session, record.task_id).unwrap();
        let second = TaskControlGuard::acquire(&session, record.task_id).unwrap();
        assert!(task_control_in_flight(&record));
        let mut replacement = record.clone();
        replacement.owner.started_at += 1;
        assert!(!task_control_in_flight(&replacement));
        replacement = record.clone();
        replacement.scope_cwd = root.path().join("other-scope");
        assert!(!task_control_in_flight(&replacement));
        drop(first);
        assert!(task_control_in_flight(&record));
        record.operations.push(OperationReceipt {
            operation_id: Uuid::new_v4(),
            request_fingerprint: Uuid::new_v4(),
            action: "steer".to_owned(),
            phase: OperationPhase::Accepted,
            outcome: record.outcome(),
        });
        drop(second);
        assert!(!task_control_in_flight(&record));
        codex_lifecycle_registry()
            .lock()
            .unwrap()
            .entries
            .remove(&owner);
    }

    #[tokio::test]
    async fn task_get_semantic_revision_tracks_usage_and_state_without_poll_churn() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("codex-revision-{}", Uuid::new_v4().simple());
        let (handle, session) = active_test_session(root.path(), &id, false).await;
        let store = TaskStore::new(root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let mut record = task_record(
            &session,
            task_id,
            TaskStatus::Running,
            7,
            Some("thread"),
            Some("turn"),
        );
        record.updated_at = record.updated_at.saturating_sub(60);
        record.verification = Some(passed_verification_at(record.revision));
        store.save(&record).unwrap();

        // An omitted turn must retain the bound turn without counting the
        // missing optional value as a public state change.
        let response = Arc::new(Mutex::new(Ok(json!({
            "thread": {"status": {"type": "active"}, "turns": []}
        }))));
        let methods = Arc::new(Mutex::new(Vec::new()));
        let client = reconciliation_client(Arc::clone(&response), Arc::clone(&methods), None);
        let lease = Arc::new(store.try_acquire_runtime_lease(task_id).unwrap().unwrap());
        insert_runtime(&session, task_id, &store, client, lease)
            .await
            .unwrap();
        for _ in 0..2 {
            let out = task_get_with_store_and_binary(
                &json!({"task_id": task_id, "after_revision": 7}),
                &session,
                &store,
                Path::new("must-not-start"),
            )
            .await
            .unwrap();
            assert_eq!(
                out,
                json!({"task_id": task_id, "status": "not_modified", "revision": 7})
            );
        }
        let unchanged = store.load(&session, task_id).unwrap();
        assert_eq!(unchanged.updated_at, record.updated_at);
        assert_eq!(unchanged.turn_id, record.turn_id);
        assert_eq!(
            task_view(&unchanged, None)["verification"]["status"],
            "passed"
        );

        for (status, expected_status, revision) in [
            ("inProgress", "running", 8),
            ("waitingApproval", "waiting_approval", 9),
            ("completed", "completed", 10),
        ] {
            *response.lock().unwrap() = Ok(json!({
                "thread": {"tokenUsage": {"totalTokens": 6}, "turns": [{"id": "turn", "status": status}]}
            }));
            let out = task_get_with_store_and_binary(
                &json!({"task_id": task_id, "after_revision": revision - 1}),
                &session,
                &store,
                Path::new("must-not-start"),
            )
            .await
            .unwrap();
            assert_eq!(out["status"], expected_status);
            assert_eq!(out["revision"], revision);
            assert_eq!(out["usage"]["total_tokens"], 6);
            assert_eq!(out["verification"]["status"], "not_run");
            assert_eq!(out["verification"]["stale"], true);
            let again = task_get_with_store_and_binary(
                &json!({"task_id": task_id, "after_revision": revision}),
                &session,
                &store,
                Path::new("must-not-start"),
            )
            .await
            .unwrap();
            assert_eq!(
                again,
                json!({"task_id": task_id, "status": "not_modified", "revision": revision})
            );
        }
        assert_eq!(methods.lock().unwrap().len(), 8);
        remove_session_with_store(&session, &store).await.unwrap();
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_get_semantic_revision_exposes_read_failure_once_and_then_recovers() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("codex-read-failure-{}", Uuid::new_v4().simple());
        let (handle, session) = active_test_session(root.path(), &id, false).await;
        let store = TaskStore::new(root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let record = task_record(
            &session,
            task_id,
            TaskStatus::Running,
            7,
            Some("thread"),
            Some("turn"),
        );
        store.save(&record).unwrap();
        let response = Arc::new(Mutex::new(
            Err("backend temporarily unavailable".to_owned()),
        ));
        let methods = Arc::new(Mutex::new(Vec::new()));
        let client = reconciliation_client(Arc::clone(&response), Arc::clone(&methods), None);
        let lease = Arc::new(store.try_acquire_runtime_lease(task_id).unwrap().unwrap());
        insert_runtime(&session, task_id, &store, client, lease)
            .await
            .unwrap();

        for (backend_response, expected_status, revision) in [
            (
                Err("backend temporarily unavailable".to_owned()),
                "unknown",
                8,
            ),
            (
                Ok(json!({"thread": {"status": {"type": "active"}, "turns": []}})),
                "running",
                9,
            ),
            (Ok(json!({"invalid": "thread response"})), "unknown", 10),
        ] {
            *response.lock().unwrap() = backend_response;
            let changed = task_get_with_store_and_binary(
                &json!({"task_id": task_id, "after_revision": revision - 1}),
                &session,
                &store,
                Path::new("must-not-start"),
            )
            .await
            .unwrap();
            assert_eq!(changed["status"], expected_status);
            assert_eq!(changed["revision"], revision);
            let unchanged = task_get_with_store_and_binary(
                &json!({"task_id": task_id, "after_revision": revision}),
                &session,
                &store,
                Path::new("must-not-start"),
            )
            .await
            .unwrap();
            assert_eq!(
                unchanged,
                json!({"task_id": task_id, "status": "not_modified", "revision": revision})
            );
        }
        assert_eq!(methods.lock().unwrap().len(), 6);
        remove_session_with_store(&session, &store).await.unwrap();
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_get_semantic_revision_discards_superseded_backend_response() {
        let root = tempfile::tempdir().unwrap();
        let id = format!("codex-read-race-{}", Uuid::new_v4().simple());
        let (handle, session) = active_test_session(root.path(), &id, false).await;
        let store = TaskStore::new(root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let record = task_record(
            &session,
            task_id,
            TaskStatus::Running,
            7,
            Some("thread"),
            Some("turn"),
        );
        store.save(&record).unwrap();
        let response = Arc::new(Mutex::new(Ok(json!({
            "thread": {"status": {"type": "active"}, "turns": []}
        }))));
        let methods = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let client = reconciliation_client(
            response,
            Arc::clone(&methods),
            Some((entered_tx, release_rx)),
        );
        let lease = Arc::new(store.try_acquire_runtime_lease(task_id).unwrap().unwrap());
        insert_runtime(&session, task_id, &store, client, lease)
            .await
            .unwrap();
        let read_session = session.clone();
        let read_store = store.clone();
        let read = tokio::spawn(async move {
            task_get_with_store_and_binary(
                &json!({"task_id": task_id, "after_revision": 7}),
                &read_session,
                &read_store,
                Path::new("must-not-start"),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), entered_rx)
            .await
            .unwrap()
            .unwrap();
        let newer = store
            .update(&session, task_id, |record| {
                record.status = TaskStatus::WaitingApproval;
                record.usage = Some(BTreeMap::from([("total_tokens".to_owned(), 6)]));
                record.revision += 1;
                Ok(())
            })
            .unwrap();
        release_tx.send(()).unwrap();
        let out = read.await.unwrap().unwrap();
        assert_eq!(out["status"], "waiting_approval");
        assert_eq!(out["revision"], newer.revision);
        assert_eq!(out["usage"]["total_tokens"], 6);
        assert_eq!(out["reconciliation_deferred"], true);
        assert!(out["evidence"].is_null());
        assert_eq!(
            store.load(&session, task_id).unwrap().revision,
            newer.revision
        );
        assert_eq!(methods.lock().unwrap().as_slice(), ["thread/read"]);
        remove_session_with_store(&session, &store).await.unwrap();
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn closing_owner_can_read_retained_task_without_restarting_runtime() {
        let root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "closing-task-read", true);
        let store = TaskStore::new(root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                task_id,
                TaskStatus::Running,
                7,
                Some("thread"),
                Some("turn"),
            ))
            .unwrap();
        let instance = SessionInstance::from_session(&owner);
        begin_session_instance_shutdown(&instance);
        let view = task_get_with_store_and_binary(
            &json!({"task_id": task_id}),
            &owner,
            &store,
            Path::new("missing-codex"),
        )
        .await
        .unwrap();
        assert_eq!(view["status"], "running");
        assert_eq!(view["recovery_state"], "owner_closing");
        assert_eq!(view["revision"], 7);
        let mut replacement = owner.clone();
        replacement.started_at += 1;
        assert!(store.load(&replacement, task_id).is_err());
        finish_session_shutdown(&instance).unwrap();
    }

    #[tokio::test]
    async fn failed_task_store_finalization_keeps_same_owner_cleanup_retryable() {
        let root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "retry-finalization", true);
        let instance = SessionInstance::from_session(&owner);
        let bad_path = root.path().join("bad-store");
        std::fs::write(&bad_path, b"not a directory").unwrap();
        begin_session_instance_shutdown(&instance);
        assert!(
            finalize_session_tasks(&instance, &TaskStore::new(bad_path))
                .await
                .is_err()
        );
        assert!(session_instance_is_closing(&instance));
        let store = TaskStore::new(root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                task_id,
                TaskStatus::Running,
                1,
                None,
                None,
            ))
            .unwrap();
        finalize_session_tasks(&instance, &store).await.unwrap();
        assert_eq!(
            store.load(&owner, task_id).unwrap().status,
            TaskStatus::Interrupted
        );
        assert!(!session_instance_is_closing(&instance));
    }

    #[tokio::test]
    async fn task_get_keeps_revision_for_unchanged_running_turn() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let (handle, owner) = active_test_session(root.path(), "stable-running-get", true).await;
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let mut record = task_record(
            &owner,
            task_id,
            TaskStatus::Running,
            7,
            Some("0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa"),
            Some("0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb"),
        );
        record.updated_at = config::unix_time().saturating_sub(10);
        store.save(&record).unwrap();
        let binary = fake_app_server(root.path(), "running-read");
        let first =
            task_get_with_store_and_binary(&json!({"task_id": task_id}), &owner, &store, &binary)
                .await
                .unwrap();
        assert_eq!(first["status"], "running");
        let revision = first["revision"].as_u64().unwrap();
        // The first probe may learn native usage. Subsequent identical probes
        // must preserve both its semantic cursor and retention timestamp.
        let observed_updated_at = store.load(&owner, task_id).unwrap().updated_at;
        let unchanged = task_get_with_store_and_binary(
            &json!({"task_id": task_id, "after_revision": revision}),
            &owner,
            &store,
            &binary,
        )
        .await
        .unwrap();
        assert_eq!(unchanged["status"], "not_modified");
        assert_eq!(unchanged["revision"], revision);
        assert_eq!(
            store.load(&owner, task_id).unwrap().updated_at,
            observed_updated_at
        );
        remove_session(&owner).await.unwrap();
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_get_reconciles_remote_completion_before_not_modified() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let (approval_sender, _approval_receiver) = approvals::approval_channel();
        let session_handle =
            approvals::spawn_runtime(root.path(), Some("reconcile-get"), true, approval_sender)
                .await
                .unwrap();
        let owner = config::read_session_metadata("reconcile-get")
            .await
            .unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let mut record = task_record(
            &owner,
            task_id,
            TaskStatus::Running,
            7,
            Some("0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa"),
            Some("0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb"),
        );
        record.operations.push(start_receipt(
            operation_id,
            fingerprint(&json!({"kind":"reconcile"})).unwrap(),
            OperationPhase::Applied,
            record.outcome(),
        ));
        store.save(&record).unwrap();
        let result = task_get_with_store_and_binary(
            &json!({
                "task_id": task_id,
                "after_revision": 7
            }),
            &owner,
            &store,
            &fake_app_server(root.path(), "reject-never"),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "completed");
        assert!(result["revision"].as_u64().unwrap() > 7);
        assert_ne!(result["status"], "not_modified");
        remove_session(&owner).await.unwrap();
        session_handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_get_defers_to_live_runtime_owner_and_resumes_after_release() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let (approval_sender, _approval_receiver) = approvals::approval_channel();
        let session_handle = approvals::spawn_runtime(
            root.path(),
            Some("runtime-owner-get"),
            true,
            approval_sender,
        )
        .await
        .unwrap();
        let owner = config::read_session_metadata("runtime-owner-get")
            .await
            .unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let task_id = Uuid::new_v4();
        let record = task_record(
            &owner,
            task_id,
            TaskStatus::Running,
            7,
            Some("0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa"),
            Some("0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb"),
        );
        store.save(&record).unwrap();
        let lease = store.try_acquire_runtime_lease(task_id).unwrap().unwrap();

        let missing_binary = root.path().join("must-not-start");
        let deferred = task_get_with_store_and_binary(
            &json!({"task_id": task_id}),
            &owner,
            &store,
            &missing_binary,
        )
        .await
        .unwrap();
        assert_eq!(deferred["status"], "running");
        assert_eq!(deferred["revision"], 7);
        assert_eq!(deferred["reconciliation_deferred"], true);
        assert_eq!(deferred["last_updated_at"], record.updated_at);
        assert_eq!(store.load(&owner, task_id).unwrap(), record);

        let not_modified = task_get_with_store_and_binary(
            &json!({"task_id": task_id, "after_revision": 7}),
            &owner,
            &store,
            &missing_binary,
        )
        .await
        .unwrap();
        assert_eq!(not_modified["status"], "not_modified");
        assert_eq!(not_modified["reconciliation_deferred"], true);
        assert_eq!(store.load(&owner, task_id).unwrap(), record);

        drop(lease);
        let reconciled = task_get_with_store_and_binary(
            &json!({"task_id": task_id}),
            &owner,
            &store,
            &fake_app_server(root.path(), "ok"),
        )
        .await
        .unwrap();
        assert_eq!(reconciled["status"], "completed");
        assert!(reconciled["revision"].as_u64().unwrap() > 7);

        remove_session_with_store(&owner, &store).await.unwrap();
        session_handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn spawned_actor_holds_runtime_lease_until_child_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(root.path().join("tasks"));
        let owner = session(root.path(), "actor-lease", true);
        let task_id = Uuid::new_v4();
        let lease = Arc::new(store.try_acquire_runtime_lease(task_id).unwrap().unwrap());
        let lease_observer = Arc::downgrade(&lease);
        let client = spawn_client_with_binary_unchecked(
            owner,
            Some(task_id),
            &fake_app_server(root.path(), "ok"),
            Some(Arc::clone(&lease)),
        )
        .unwrap();

        drop(lease);
        assert!(lease_observer.upgrade().is_some());
        assert!(store.try_acquire_runtime_lease(task_id).unwrap().is_none());

        client.shutdown().await;
        assert!(lease_observer.upgrade().is_none());
        assert!(store.try_acquire_runtime_lease(task_id).unwrap().is_some());
    }

    #[tokio::test]
    async fn pre_thread_startup_failure_can_retry_same_operation() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "retryable-start", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let operation_id = Uuid::new_v4();
        let args = json!({
            "operation_id": operation_id,
            "task": "retry after startup",
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let missing = root.path().join("does-not-exist");
        let first = task_start_with_store_and_binary(&args, &owner, &store, &missing)
            .await
            .unwrap();
        assert_eq!(first["status"], "retryable_failed");
        assert_eq!(
            store
                .load(&owner, task_id_for_operation(&owner, operation_id).unwrap())
                .unwrap()
                .operations[0]
                .phase,
            OperationPhase::RetryableFailed
        );

        let binary = fake_app_server(root.path(), "ok");
        let second = task_start_with_store_and_binary(&args, &owner, &store, &binary)
            .await
            .unwrap();
        assert_eq!(second["status"], "running");
        assert!(second["thread_id"].as_str().is_some());
        remove_session(&owner).await.unwrap();
    }

    #[tokio::test]
    async fn pre_thread_model_failure_retries_but_invalid_model_is_deterministic() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "retryable-model", true);
        let store = TaskStore::new(store_root.path().join("tasks"));

        let transient_id = Uuid::new_v4();
        let transient_args = json!({
            "operation_id": transient_id,
            "task": "retry model list",
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let first = task_start_with_store_and_binary(
            &transient_args,
            &owner,
            &store,
            &fake_app_server(root.path(), "model-fail"),
        )
        .await
        .unwrap();
        assert_eq!(first["status"], "retryable_failed");
        let retry = task_start_with_store_and_binary(
            &transient_args,
            &owner,
            &store,
            &fake_app_server(root.path(), "ok"),
        )
        .await
        .unwrap();
        assert_eq!(retry["status"], "running");
        remove_session(&owner).await.unwrap();

        let deterministic_id = Uuid::new_v4();
        let deterministic_args = json!({
            "operation_id": deterministic_id,
            "task": "invalid model",
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let failed = task_start_with_store_and_binary(
            &deterministic_args,
            &owner,
            &store,
            &fake_app_server(root.path(), "invalid-model"),
        )
        .await
        .unwrap();
        assert_eq!(failed["status"], "failed");
        let replay = task_start_with_store_and_binary(
            &deterministic_args,
            &owner,
            &store,
            &root.path().join("never-spawned"),
        )
        .await
        .unwrap();
        assert_eq!(replay["status"], "failed");
    }

    #[tokio::test]
    async fn uncertain_thread_start_failure_is_not_blindly_replayed() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let owner = session(root.path(), "uncertain-thread", true);
        let store = TaskStore::new(store_root.path().join("tasks"));
        let operation_id = Uuid::new_v4();
        let args = json!({
            "operation_id": operation_id,
            "task": "uncertain thread",
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let first = task_start_with_store_and_binary(
            &args,
            &owner,
            &store,
            &fake_app_server(root.path(), "thread-uncertain"),
        )
        .await
        .unwrap();
        assert_eq!(first["status"], "reconciliation_required");
        let retry = task_start_with_store_and_binary(
            &args,
            &owner,
            &store,
            &fake_app_server(root.path(), "ok"),
        )
        .await
        .unwrap();
        assert_eq!(retry["status"], "reconciliation_required");
        assert!(
            runtime_for(&owner, task_id_for_operation(&owner, operation_id).unwrap()).is_none()
        );
    }

    #[test]
    fn child_approval_detail_identifies_safe_action_without_raw_arguments() {
        let task_id = Uuid::from_u128(1);
        let marker = "secret-command-argument";
        let (command_detail, command_metadata) = child_approval(
            "command execution",
            "commandExecution",
            "command_execution",
            task_id,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "command": ["cargo", "test", marker, "--package=private"]
            }),
        );
        assert!(command_detail.contains("operation: command execution"));
        assert!(command_detail.contains("executable cargo"));
        assert!(command_detail.contains("option names: --package"));
        assert!(!command_detail.contains(marker));
        assert_eq!(command_metadata["provenance"], "codex_app_server");
        assert_eq!(
            command_metadata["tool"],
            "item/commandExecution/requestApproval"
        );
        assert_eq!(command_metadata["command"], "argument_values_omitted");

        let (file_detail, file_metadata) = child_approval(
            "file change",
            "fileChange",
            "file_change",
            task_id,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-2",
                "changes": [{"path": marker, "diff": "private patch"}]
            }),
        );
        assert!(file_detail.contains("file changes: 1 change(s)"));
        assert!(file_detail.contains("paths and patch details omitted"));
        assert!(!file_detail.contains(marker));
        assert_eq!(file_metadata["change_count"], "1");
        assert_eq!(file_metadata["change_details"], "omitted");
    }

    #[test]
    fn task_start_only_treats_the_explicit_missing_marker_as_absence() {
        assert!(is_missing_task(&anyhow::anyhow!(
            "CODEX_TASK_NOT_FOUND: task was not found"
        )));
        assert!(!is_missing_task(&anyhow::anyhow!(
            "invalid Codex task record"
        )));
    }

    #[test]
    fn model_list_effort_parser_accepts_reasoning_effort_schema_aliases() {
        let models = json!({
            "data": [{
                "model": "gpt-5.6-luna",
                "supportedReasoningEfforts": [
                    {"reasoningEffort": "low", "description": "low"},
                    {"reasoningEffort": "max", "description": "max"},
                ],
            }]
        });
        validate_model_request(&models, "gpt-5.6-luna", "max").unwrap();
        assert!(validate_model_request(&models, "gpt-5.6-luna", "ultra").is_err());
        assert_eq!(
            advertised_effort_name(&json!({"reasoningEffort": "max"})),
            Some("max")
        );
        assert_eq!(
            advertised_effort_name(&json!({"effort": "high"})),
            Some("high")
        );
        assert_eq!(advertised_effort_name(&json!("medium")), Some("medium"));
    }

    #[test]
    fn model_inventory_preserves_native_selection_metadata_without_inventing_legacy_defaults() {
        let legacy = json!({"model":"legacy", "supportedReasoningEfforts":["medium"]});
        assert_eq!(
            advertised_model(&legacy).unwrap(),
            json!({"model":"legacy", "efforts":["medium"]})
        );
        let mut current = legacy;
        current["hidden"] = json!(false);
        current["isDefault"] = json!(true);
        current["defaultReasoningEffort"] = json!("medium");
        assert_eq!(
            advertised_model(&current).unwrap(),
            json!({"model":"legacy", "efforts":["medium"], "hidden":false, "is_default":true, "default_effort":"medium"})
        );
    }

    fn initialize_response(user_agent: &str) -> Value {
        json!({
            "userAgent": user_agent,
            "codexHome": "/tmp/codex",
            "platformFamily": "unix",
            "platformOs": "linux",
        })
    }

    #[test]
    fn initialize_validation_is_version_agnostic() {
        for user_agent in [
            format!(
                "temote-mcp/0.147.0 (Ubuntu 24.4.0; x86_64) unknown (temote-mcp; {APP_SERVER_CLIENT_VERSION})"
            ),
            format!(
                "temote-mcp/0.153.4 (macOS 15.6; aarch64) dumb (temote-mcp; {APP_SERVER_CLIENT_VERSION})"
            ),
            format!(
                "temote-mcp/99.123.456 (FutureOS 1; x86_64) future (temote-mcp; {APP_SERVER_CLIENT_VERSION})"
            ),
            "future-codex-app-server build-2027-01".to_owned(),
        ] {
            validate_initialize_response(&initialize_response(&user_agent)).unwrap();
        }
    }

    #[test]
    fn generated_initialize_versions_are_not_allowlisted() -> noprop::TestResult {
        crate::test_support::run(0x434f_4445_5856_4552, 1024, |ctx| {
            let version = format!(
                "{}.{}.{}",
                noprop::sample_u32(ctx),
                noprop::sample_u32(ctx),
                noprop::sample_u32(ctx)
            );
            let user_agent = format!(
                "temote-mcp/{version} (FutureOS 1; x86_64) future (temote-mcp; {APP_SERVER_CLIENT_VERSION})"
            );
            let initialized = initialize_response(&user_agent);
            validate_initialize_response(&initialized).unwrap();
            assert_eq!(
                app_server_version_from_initialize_response(&initialized),
                Some(version.as_str())
            );
            Ok(())
        })
    }

    #[test]
    fn app_server_version_is_best_effort_diagnostic_only() {
        let known_shape = format!(
            "temote-mcp/99.123.456 (FutureOS 1; x86_64) future (temote-mcp; {APP_SERVER_CLIENT_VERSION})"
        );
        let unknown_shape = "future-codex-app-server build-2027-01";
        assert_eq!(
            app_server_version_from_initialize_response(&initialize_response(&known_shape)),
            Some("99.123.456")
        );
        assert_eq!(
            app_server_version_from_initialize_response(&initialize_response(unknown_shape)),
            None
        );
        validate_initialize_response(&initialize_response(unknown_shape)).unwrap();
    }

    #[test]
    fn initialize_validation_bounds_user_agent_without_pinning_grammar() {
        let mut empty = initialize_response("");
        assert!(validate_initialize_response(&empty).is_err());

        empty["userAgent"] = json!("x".repeat(MAX_APP_SERVER_USER_AGENT_BYTES + 1));
        assert!(validate_initialize_response(&empty).is_err());

        empty["userAgent"] = json!(7);
        assert!(validate_initialize_response(&empty).is_err());
    }

    #[test]
    fn initialize_validation_requires_the_response_shape() {
        let user_agent = format!(
            "temote-mcp/0.153.4 (Ubuntu 24.4.0; x86_64) unknown (temote-mcp; {APP_SERVER_CLIENT_VERSION})"
        );
        let valid = initialize_response(&user_agent);

        for field in ["userAgent", "codexHome", "platformFamily", "platformOs"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                validate_initialize_response(&missing).is_err(),
                "missing {field} was accepted"
            );

            for invalid_value in [Value::Null, json!(7)] {
                let mut wrong_type = valid.clone();
                wrong_type[field] = invalid_value;
                assert!(
                    validate_initialize_response(&wrong_type).is_err(),
                    "non-string {field} was accepted"
                );
            }
        }

        let mut relative_home = valid.clone();
        relative_home["codexHome"] = json!("relative/codex-home");
        assert!(validate_initialize_response(&relative_home).is_err());

        let mut empty_platform = valid;
        empty_platform["platformOs"] = json!("");
        assert!(validate_initialize_response(&empty_platform).is_err());
    }

    #[tokio::test]
    async fn fake_app_server_handshake_start_read_steer_interrupt_and_evidence() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "fake", true);
        let binary = fake_app_server(root.path(), "ok");
        let task_id = Uuid::new_v4();
        let (client, initialized) = spawn_initialized_client_with_binary_mode(
            &session,
            Some(task_id),
            &binary,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(initialized["platformOs"], "linux");
        let models = client
            .request("model/list", json!({"includeHidden":true}))
            .await
            .unwrap();
        validate_model_request(&models, "gpt-5.6-luna", "max").unwrap();
        let thread = client
            .request(
                "thread/start",
                json!({"cwd":session.cwd,"sandbox":"workspace-write"}),
            )
            .await
            .unwrap();
        let thread_id = thread["thread"]["id"].as_str().unwrap().to_owned();
        let turn = client
            .request(
                "turn/start",
                json!({"threadId":thread_id,"input":[{"type":"text","text":"x"}]}),
            )
            .await
            .unwrap();
        let turn_id = turn["turn"]["id"].as_str().unwrap().to_owned();
        assert!(client.request("turn/steer", json!({"threadId":thread_id,"expectedTurnId":turn_id,"input":[{"type":"text","text":"y"}]})).await.is_ok());
        assert!(
            client
                .request(
                    "turn/interrupt",
                    json!({"threadId":thread_id,"turnId":turn_id})
                )
                .await
                .is_ok()
        );
        let read = client
            .request(
                "thread/read",
                json!({"threadId":thread_id,"includeTurns":true}),
            )
            .await
            .unwrap();
        let derived = derive_thread_state(&read, Some(&turn_id)).unwrap();
        assert_eq!(derived.status, TaskStatus::Completed);
        assert_eq!(derived.usage.as_ref().unwrap()["input_tokens"], 4);
        assert_eq!(derived.usage.as_ref().unwrap()["total_tokens"], 6);
        let reference = evidence::store(
            &session.id,
            &session.cwd,
            serde_json::to_string(&read).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(Uuid::parse_str(&reference.evidence_id).is_ok());
        client.shutdown().await;
    }

    #[tokio::test]
    async fn app_server_accepts_arbitrary_peer_version_and_rejects_oversized_protocol() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "protocol", true);
        let arbitrary_version = root.path().join("fake-app-server-arbitrary-version");
        let arbitrary_version_script = "#!/usr/bin/env python3\nimport json,sys\nfor line in sys.stdin:\n r=json.loads(line)\n if r.get('method')=='initialize': print(json.dumps({'id':r['id'],'result':{'userAgent':'temote-mcp/99.123.456 (FutureOS 1; x86_64) future (temote-mcp; __CLIENT_VERSION__)','codexHome':'/tmp','platformFamily':'unix','platformOs':'linux'}}),flush=True)\n"
            .replace("__CLIENT_VERSION__", APP_SERVER_CLIENT_VERSION);
        std::fs::write(&arbitrary_version, arbitrary_version_script).unwrap();
        std::fs::set_permissions(&arbitrary_version, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let (client, initialized) = spawn_initialized_client_with_binary_mode(
            &session,
            None,
            &arbitrary_version,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            app_server_version_from_initialize_response(&initialized),
            Some("99.123.456")
        );
        client.shutdown().await;

        let oversized = root.path().join("fake-app-server-oversized");
        std::fs::write(
            &oversized,
            format!("#!/usr/bin/env python3\nimport sys,json\nfor line in sys.stdin:\n sys.stdout.write('x'*{}+'\\n');sys.stdout.flush()\n", MAX_RPC_LINE_BYTES + 1),
        )
        .unwrap();
        std::fs::set_permissions(&oversized, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error =
            spawn_initialized_client_with_binary_mode(&session, None, &oversized, false, None)
                .await
                .err()
                .unwrap();
        assert!(error.to_string().contains("message exceeds"));
    }

    #[test]
    fn codex_app_server_environment_filter_is_deliberate() {
        let filtered = filtered_codex_environment([
            (OsString::from("HOME"), OsString::from("/home/test")),
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("OPENAI_API_KEY"), OsString::from("secret")),
            (OsString::from("TEMOTE_SECRET"), OsString::from("no")),
            (
                OsString::from("AWS_SECRET_ACCESS_KEY"),
                OsString::from("no"),
            ),
        ]);
        let keys = filtered
            .into_iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(keys.contains(&"HOME".to_owned()));
        assert!(keys.contains(&"PATH".to_owned()));
        assert!(keys.contains(&"LC_ALL".to_owned()));
        assert!(keys.contains(&"OPENAI_API_KEY".to_owned()));
        assert!(!keys.contains(&"TEMOTE_SECRET".to_owned()));
        assert!(!keys.contains(&"AWS_SECRET_ACCESS_KEY".to_owned()));
    }

    // ---------- session-owned task listing ----------

    #[tokio::test]
    async fn pending_interaction_owner_projection_is_bounded_and_instance_fenced() {
        let workspace = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(workspace.path(), "pending-owner", false);
        let task_id = Uuid::new_v4();
        let mut record = task_record(
            &owner,
            task_id,
            TaskStatus::Running,
            7,
            Some("thread"),
            Some("turn"),
        );
        let methods = Arc::new(Mutex::new(Vec::new()));
        let client = recording_client(Arc::clone(&methods));
        client.mark_pending_summary_ready(&record);
        record.operations.push(start_receipt(
            Uuid::new_v4(),
            Uuid::new_v4(),
            OperationPhase::Applied,
            record.outcome(),
        ));
        store.save(&record).unwrap();

        client.pending_approvals.store(2, Ordering::Release);
        let old_instance_id = Uuid::new_v4();
        *client.runtime_instance_id.lock().unwrap() = Some(old_instance_id);
        let old_runtime_lease = test_runtime_lease(store_root.path(), task_id);
        runtimes().lock().unwrap().insert(
            task_id,
            RuntimeHandle {
                client: client.clone(),
                owner: SessionInstance::from_session(&owner),
                scope: owner.cwd.clone(),
                instance_id: old_instance_id,
                started_at: Instant::now(),
                _lease: old_runtime_lease,
            },
        );

        let expired_task_id = Uuid::new_v4();
        let mut expired = task_record(
            &owner,
            expired_task_id,
            TaskStatus::Completed,
            1,
            None,
            None,
        );
        expired.updated_at = config::unix_time().saturating_sub(TASK_RETENTION_SECONDS);
        store.save(&expired).unwrap();

        let original = store.read_record(task_id).unwrap();
        let failed_permit: std::future::Ready<
            Result<crate::pending_interaction::HostObservationPermit>,
        > = std::future::ready(Err(anyhow::anyhow!("slot unavailable")));
        assert!(
            observe_pending_interaction_step_with(
                &owner,
                task_id,
                &store,
                old_instance_id,
                Duration::from_millis(10),
                failed_permit,
            )
            .await
        );
        assert!(runtime_registration_matches(
            &owner,
            task_id,
            old_instance_id
        ));
        assert!(client.is_connected());

        assert!(
            store
                .observe_pending_interaction(&owner, task_id, old_instance_id, Some(1))
                .unwrap()
        );
        let pending = store.read_record(task_id).unwrap();
        let summary = pending.pending_interaction.as_ref().unwrap();
        assert_eq!(summary.state, SummaryState::Pending);
        assert_eq!(summary.count, Some(2));
        assert_eq!(summary.types, vec![InteractionType::Approval]);
        assert_eq!(summary.summary_revision, 1);
        assert_eq!(pending.created_at, original.created_at);
        assert_eq!(pending.updated_at, original.updated_at);
        assert_eq!(pending.status, original.status);
        assert_eq!(pending.revision, original.revision);
        assert_eq!(pending.generation, original.generation);
        assert_eq!(pending.operations, original.operations);

        let listed = task_list_with_store(&json!({}), &owner, &store).unwrap();
        assert_eq!(
            listed["tasks"][0]["pending_interaction"]["state"],
            "pending"
        );
        assert_eq!(listed["tasks"][0]["pending_interaction"]["count"], 2);
        assert!(methods.lock().unwrap().is_empty());

        client.pending_approvals.store(65, Ordering::Release);
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, old_instance_id, Some(1))
                .unwrap()
        );
        let bounded = store.read_record(task_id).unwrap();
        let bounded_summary = bounded.pending_interaction.as_ref().unwrap();
        assert_eq!(bounded_summary.state, SummaryState::Unavailable);
        assert_eq!(bounded_summary.count, Some(64));
        assert!(bounded_summary.truncated);
        assert_eq!(bounded_summary.summary_revision, 2);

        client.pending_approvals.store(0, Ordering::Release);
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, old_instance_id, Some(1))
                .unwrap()
        );
        let resolved = store.read_record(task_id).unwrap();
        let resolved_summary = resolved.pending_interaction.as_ref().unwrap();
        assert_eq!(resolved_summary.state, SummaryState::None);
        assert_eq!(resolved_summary.count, Some(0));
        assert_eq!(resolved_summary.summary_revision, 3);
        assert_eq!(resolved.revision, original.revision);
        assert_eq!(resolved.updated_at, original.updated_at);
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, old_instance_id, Some(1))
                .unwrap()
        );
        assert!(store.read_record(expired_task_id).is_ok());
        let listed = task_list_with_store(&json!({}), &owner, &store).unwrap();
        let projected = listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["task_id"] == json!(task_id))
            .unwrap();
        assert_eq!(projected["pending_interaction"]["state"], "none");
        assert!(methods.lock().unwrap().is_empty());

        let stale_summary = Summary::observe(
            None,
            SummaryState::Pending,
            Some(1),
            &[InteractionType::Approval],
            false,
            ProducerKind::RuntimeOwner,
            1,
            config::unix_time().saturating_sub(crate::pending_interaction::SUMMARY_TTL_SECS + 1),
        )
        .unwrap();
        let mut expired_projection = store.read_record(task_id).unwrap();
        expired_projection.pending_interaction = Some(stale_summary);
        store.save(&expired_projection).unwrap();
        let listed = task_list_with_store(&json!({}), &owner, &store).unwrap();
        let projected = listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["task_id"] == json!(task_id))
            .unwrap();
        assert_eq!(projected["pending_interaction"]["state"], "unavailable");
        assert_eq!(
            store.read_record(task_id).unwrap().pending_interaction,
            expired_projection.pending_interaction
        );
        let mut out_of_retention = store.read_record(task_id).unwrap();
        out_of_retention.updated_at = config::unix_time().saturating_sub(TASK_RETENTION_SECONDS);
        store.save(&out_of_retention).unwrap();
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, old_instance_id, Some(1))
                .is_ok_and(|observed| !observed)
        );
        assert_eq!(
            store.read_record(task_id).unwrap().pending_interaction,
            out_of_retention.pending_interaction
        );

        let replacement_methods = Arc::new(Mutex::new(Vec::new()));
        let replacement_client = recording_client(replacement_methods);
        let replacement_instance_id = Uuid::new_v4();
        *replacement_client.runtime_instance_id.lock().unwrap() = Some(replacement_instance_id);
        let mut replacement_record = store.read_record(task_id).unwrap();
        replacement_record.generation = 2;
        store.save(&replacement_record).unwrap();
        let old_runtime = take_runtime_if_instance(task_id, old_instance_id).unwrap();
        drop(old_runtime);
        let replacement_runtime_lease = test_runtime_lease(store_root.path(), task_id);
        runtimes().lock().unwrap().insert(
            task_id,
            RuntimeHandle {
                client: replacement_client.clone(),
                owner: SessionInstance::from_session(&owner),
                scope: owner.cwd.clone(),
                instance_id: replacement_instance_id,
                started_at: Instant::now(),
                _lease: replacement_runtime_lease,
            },
        );

        assert!(
            !store
                .observe_pending_interaction(&owner, task_id, old_instance_id, None)
                .unwrap()
        );
        assert!(take_runtime_if_instance(task_id, old_instance_id).is_none());
        assert!(runtime_registration_matches(
            &owner,
            task_id,
            replacement_instance_id
        ));
        assert_eq!(
            runtimes()
                .lock()
                .unwrap()
                .get(&task_id)
                .unwrap()
                .instance_id,
            replacement_instance_id
        );

        let stale_epoch_record = store.read_record(task_id).unwrap();
        let mut retired = stale_epoch_record.clone();
        retired.generation = 0;
        retired.updated_at = config::unix_time();
        store.save(&retired).unwrap();
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, replacement_instance_id, Some(0))
                .unwrap()
        );
        assert_eq!(
            store.read_record(task_id).unwrap().pending_interaction,
            stale_epoch_record.pending_interaction
        );

        let _ = take_runtime_if_instance(task_id, replacement_instance_id);
        client.shutdown().await;
        replacement_client.shutdown().await;
    }

    #[tokio::test]
    async fn pending_interaction_reconnect_read_cannot_restore_none_without_in_process_proof() {
        let workspace = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(workspace.path(), "pending-reconnect-owner", false);
        let task_id = Uuid::new_v4();
        let old_observed_at =
            config::unix_time().saturating_sub(crate::pending_interaction::SUMMARY_TTL_SECS + 5);
        let mut record = task_record(
            &owner,
            task_id,
            TaskStatus::Running,
            3,
            Some("stored-thread"),
            Some("stored-turn"),
        );
        record.pending_interaction = Some(
            Summary::observe(
                None,
                SummaryState::None,
                Some(0),
                &[],
                false,
                ProducerKind::RuntimeOwner,
                record.generation,
                old_observed_at,
            )
            .unwrap(),
        );
        store.save(&record).unwrap();

        let methods = Arc::new(Mutex::new(Vec::new()));
        let client = recording_client(methods);
        client.pending_approvals.store(0, Ordering::Release);
        let runtime_instance_id = Uuid::new_v4();
        *client.runtime_instance_id.lock().unwrap() = Some(runtime_instance_id);
        let runtime_lease = test_runtime_lease(store_root.path(), task_id);
        runtimes().lock().unwrap().insert(
            task_id,
            RuntimeHandle {
                client: client.clone(),
                owner: SessionInstance::from_session(&owner),
                scope: owner.cwd.clone(),
                instance_id: runtime_instance_id,
                started_at: Instant::now(),
                _lease: runtime_lease,
            },
        );

        assert!(
            store
                .observe_pending_interaction(&owner, task_id, runtime_instance_id, Some(1))
                .unwrap()
        );
        let unloaded = store.read_record(task_id).unwrap();
        let unavailable = unloaded.pending_interaction.as_ref().unwrap();
        assert_eq!(unavailable.state, SummaryState::Unavailable);
        assert_eq!(unavailable.observed_at, old_observed_at);
        assert_eq!(
            unavailable.expires_at,
            old_observed_at + crate::pending_interaction::SUMMARY_TTL_SECS
        );
        assert!(client.is_connected());

        let mut wrong_binding_record = record.clone();
        wrong_binding_record.turn_id = Some("different-turn".to_owned());
        client.mark_pending_summary_ready(&wrong_binding_record);
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, runtime_instance_id, Some(1))
                .unwrap()
        );
        let mismatched = store.read_record(task_id).unwrap();
        let unavailable = mismatched.pending_interaction.as_ref().unwrap();
        assert_eq!(unavailable.state, SummaryState::Unavailable);
        assert_eq!(unavailable.observed_at, old_observed_at);
        assert!(client.is_connected());

        let matching_read = json!({
            "thread": {
                "id": "stored-thread",
                "status": {"type": "active", "activeFlags": ["waitingOnApproval"]},
                "turns": [{"id": "stored-turn", "status": "inProgress"}]
            }
        });
        let derived = derive_thread_state(&matching_read, Some("stored-turn")).unwrap();
        assert_eq!(derived.status, TaskStatus::Running);
        assert_eq!(derived.turn_id.as_deref(), Some("stored-turn"));
        *client.pending_summary_binding.lock().unwrap() = None;
        assert!(!client.pending_summary_ready_for(&record));
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, runtime_instance_id, Some(1))
                .unwrap()
        );
        let after_read = store.read_record(task_id).unwrap();
        let unavailable = after_read.pending_interaction.as_ref().unwrap();
        assert_eq!(unavailable.state, SummaryState::Unavailable);
        assert_eq!(unavailable.observed_at, old_observed_at);
        assert_eq!(unavailable.summary_revision, 2);

        client.pending_approvals.store(1, Ordering::Release);
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, runtime_instance_id, Some(1))
                .unwrap()
        );
        let pending = store.read_record(task_id).unwrap();
        let summary = pending.pending_interaction.as_ref().unwrap();
        assert_eq!(summary.state, SummaryState::Pending);
        assert_eq!(summary.count, Some(1));
        assert_eq!(summary.summary_revision, 3);

        client.pending_approvals.store(0, Ordering::Release);
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, runtime_instance_id, Some(1))
                .unwrap()
        );
        let after_resolution = store.read_record(task_id).unwrap();
        let unavailable = after_resolution.pending_interaction.as_ref().unwrap();
        assert_eq!(unavailable.state, SummaryState::Unavailable);
        assert_eq!(unavailable.observed_at, summary.observed_at);
        assert_eq!(unavailable.summary_revision, 4);

        store
            .update(&owner, task_id, |record| {
                record.status = TaskStatus::WaitingApproval;
                Ok(())
            })
            .unwrap();
        assert!(
            store
                .observe_pending_interaction(&owner, task_id, runtime_instance_id, Some(1))
                .unwrap()
        );
        let waiting = store.read_record(task_id).unwrap();
        let unavailable = waiting.pending_interaction.as_ref().unwrap();
        assert_eq!(unavailable.state, SummaryState::Unavailable);
        assert_eq!(unavailable.observed_at, summary.observed_at);
        assert!(client.is_connected());

        let _ = take_runtime_if_instance(task_id, runtime_instance_id);
        client.shutdown().await;
    }

    #[test]
    fn task_list_projects_only_the_calling_sessions_tasks() {
        let workspace = tempfile::tempdir().unwrap();
        let other_workspace = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(workspace.path(), "list-owner", true);
        let other = session(workspace.path(), "list-other", true);
        // Same id under a different instance is still a different
        // session; a different scope is a different task list entirely.
        let stale = config::Session {
            started_at: 9999,
            ..owner.clone()
        };
        let elsewhere = session(other_workspace.path(), "list-owner", true);
        let owner_task = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                owner_task,
                TaskStatus::Running,
                1,
                None,
                None,
            ))
            .unwrap();
        for session in [&other, &stale, &elsewhere] {
            store
                .save(&task_record(
                    session,
                    Uuid::new_v4(),
                    TaskStatus::Running,
                    1,
                    None,
                    None,
                ))
                .unwrap();
        }

        let view = task_list_with_store(&json!({}), &owner, &store).unwrap();
        assert_eq!(view["backend"], "codex");
        assert_eq!(view["total"], 1);
        assert_eq!(view["skipped"], 0);
        assert_eq!(view["truncated"], false);
        let tasks = view["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["task_id"], json!(owner_task));
        assert_eq!(tasks[0]["backend"], "codex");
        assert!(tasks[0]["last_updated_at"].is_u64());
    }

    #[test]
    fn task_list_counts_unreadable_records_as_skipped() {
        let workspace = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(workspace.path(), "list-skipped", true);
        store
            .save(&task_record(
                &owner,
                Uuid::new_v4(),
                TaskStatus::Running,
                1,
                None,
                None,
            ))
            .unwrap();
        // A record file that fails to parse cannot be verified as owned:
        // it is reported as skipped, not silently dropped.
        let corrupt = store_root
            .path()
            .join("tasks")
            .join(format!("{}.json", Uuid::new_v4()));
        std::fs::write(&corrupt, "{ not json").unwrap();

        let view = task_list_with_store(&json!({}), &owner, &store).unwrap();
        assert_eq!(view["total"], 1);
        assert_eq!(view["skipped"], 1);
        assert_eq!(view["tasks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn task_list_orders_newest_first_and_truncates_at_limit() {
        let workspace = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(workspace.path(), "list-order", true);
        let now = config::unix_time();
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        for (index, task_id) in ids.iter().enumerate() {
            let mut record = task_record(&owner, *task_id, TaskStatus::Running, 1, None, None);
            record.created_at = now - 100;
            record.updated_at = now - index as u64;
            store.save(&record).unwrap();
        }

        let view = task_list_with_store(&json!({"limit": 2}), &owner, &store).unwrap();
        assert_eq!(view["total"], 3);
        assert_eq!(view["truncated"], true);
        let tasks = view["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["task_id"], json!(ids[0]));
        assert_eq!(tasks[1]["task_id"], json!(ids[1]));
    }

    #[test]
    fn task_list_reports_an_empty_store_as_empty() {
        let workspace = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        // The directory does not exist until the first record lands.
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(workspace.path(), "list-empty", true);

        let view = task_list_with_store(&json!({}), &owner, &store).unwrap();
        assert_eq!(view["tasks"].as_array().unwrap().len(), 0);
        assert_eq!(view["total"], 0);
        assert_eq!(view["skipped"], 0);
        assert_eq!(view["truncated"], false);
    }

    #[test]
    fn task_list_rejects_invalid_limits() {
        let workspace = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = TaskStore::new(store_root.path().join("tasks"));
        let owner = session(workspace.path(), "list-limit", true);

        assert_eq!(
            task_list_with_store(&json!({"limit": 0}), &owner, &store)
                .unwrap_err()
                .to_string(),
            "task_list limit must be 1..=128"
        );
        assert_eq!(
            task_list_with_store(&json!({"limit": 129}), &owner, &store)
                .unwrap_err()
                .to_string(),
            "task_list limit must be 1..=128"
        );
        assert_eq!(
            task_list_with_store(&json!({"limit": "many"}), &owner, &store)
                .unwrap_err()
                .to_string(),
            "task_list limit must be an integer"
        );
    }

    // ---------- separated execution / verification / delivery state (A4) ----------

    fn passed_verification_at(record_revision: u64) -> VerificationRecord {
        VerificationRecord {
            status: outcome::VerificationStatus::Passed,
            target: outcome::VerificationTarget::Commit {
                commit: "abc123".to_owned(),
            },
            record_revision,
            checked_at: 1_700_000_000,
        }
    }

    fn submitted_delivery() -> DeliveryRecord {
        DeliveryRecord {
            status: outcome::DeliveryStatus::Submitted,
            branch: Some("feat/a4".to_owned()),
            pull_request: Some("https://example.invalid/pr/1".to_owned()),
            updated_at: 1_700_000_001,
        }
    }

    #[test]
    fn task_view_separates_execution_from_verification_and_delivery() {
        let workspace = tempfile::tempdir().unwrap();
        let owner = session(workspace.path(), "state-separation", true);
        let task_id = Uuid::new_v4();
        let mut record = task_record(
            &owner,
            task_id,
            TaskStatus::Completed,
            3,
            Some("thread-1"),
            Some("turn-1"),
        );

        // A completed execution alone is not a verification PASS and not a
        // delivery.
        let view = task_view(&record, None);
        assert_eq!(view["status"], "completed");
        assert_eq!(view["execution"]["state"], "completed");
        assert_eq!(view["execution"]["generation"], record.generation);
        assert_eq!(
            view["execution"]["id"],
            outcome::execution_id(task_id, record.generation).to_string()
        );
        assert_eq!(view["verification"]["status"], "not_run");
        assert_eq!(view["verification"]["stale"], false);
        assert_eq!(view["delivery"]["status"], "not_started");

        // Recorded states are reported verbatim while they apply.
        record.verification = Some(passed_verification_at(record.revision));
        record.delivery = Some(submitted_delivery());
        let view = task_view(&record, None);
        assert_eq!(view["verification"]["status"], "passed");
        assert_eq!(view["verification"]["stale"], false);
        assert_eq!(view["delivery"]["status"], "submitted");
        assert_eq!(view["delivery"]["branch"], "feat/a4");
    }

    #[test]
    fn legacy_records_without_outcome_fields_read_as_not_run() {
        let workspace = tempfile::tempdir().unwrap();
        let owner = session(workspace.path(), "legacy-state", true);
        let record = task_record(
            &owner,
            Uuid::new_v4(),
            TaskStatus::Completed,
            3,
            Some("thread-1"),
            Some("turn-1"),
        );

        // Current records write both fields, and a pre-A4 record without
        // them deserializes through the serde defaults: no migration step.
        let mut value = serde_json::to_value(&record).unwrap();
        let object = value.as_object_mut().unwrap();
        assert!(object.remove("verification").is_some());
        assert!(object.remove("delivery").is_some());
        assert!(object.remove("continued_from_task_id").is_some());
        assert!(object.remove("continued_by_task_id").is_some());
        assert!(object.remove("continued_by_request_fingerprint").is_some());
        let legacy: TaskRecord = serde_json::from_value(value).unwrap();
        assert!(legacy.verification.is_none());
        assert!(legacy.delivery.is_none());
        assert!(legacy.continued_from_task_id.is_none());
        assert!(legacy.continued_by_task_id.is_none());

        let view = task_view(&legacy, None);
        assert_eq!(view["verification"]["status"], "not_run");
        assert_eq!(view["delivery"]["status"], "not_started");
    }

    #[test]
    fn stale_verification_is_not_reported_as_a_current_pass() {
        let workspace = tempfile::tempdir().unwrap();
        let owner = session(workspace.path(), "stale-state", true);
        let mut record = task_record(
            &owner,
            Uuid::new_v4(),
            TaskStatus::Completed,
            3,
            Some("thread-1"),
            Some("turn-1"),
        );
        record.verification = Some(passed_verification_at(2));

        let view = task_view(&record, None);
        assert_eq!(view["status"], "completed");
        assert_eq!(view["verification"]["status"], "not_run");
        assert_eq!(view["verification"]["stale"], true);
        assert_eq!(view["verification"]["record_revision"], 2);
        assert_eq!(
            view["verification"]["target"]["commit"], "abc123",
            "the stale result stays visible without being a current PASS"
        );
    }

    #[test]
    fn validation_rejects_out_of_contract_outcome_records() {
        let workspace = tempfile::tempdir().unwrap();
        let owner = session(workspace.path(), "invalid-state", true);
        let mut record = task_record(&owner, Uuid::new_v4(), TaskStatus::Completed, 3, None, None);
        record.verification = Some(VerificationRecord {
            status: outcome::VerificationStatus::Passed,
            target: outcome::VerificationTarget::Commit {
                commit: String::new(),
            },
            record_revision: 3,
            checked_at: 1_700_000_000,
        });
        assert!(validate_record(&record).is_err());
    }

    #[tokio::test]
    async fn continued_start_resumes_once_and_preserves_independent_task_state() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let owner = session(workspace.path(), "continuation-fixture", true);
        let store = TaskStore::new(state.path().join("tasks"));
        let binary = fake_app_server(state.path(), "continuation");
        let source_operation = Uuid::new_v4();
        let source_args = json!({"operation_id":source_operation,"task":"first","model":"gpt-5.6-luna","effort":"high"});
        let source_view = task_start_with_store_and_binary(&source_args, &owner, &store, &binary)
            .await
            .unwrap();
        let source_id = Uuid::parse_str(source_view["task_id"].as_str().unwrap()).unwrap();
        store
            .update(&owner, source_id, |source| {
                source.status = TaskStatus::Completed;
                source.revision += 1;
                Ok(())
            })
            .unwrap();
        let successor_operation = Uuid::new_v4();
        let args = json!({"operation_id":successor_operation,"task":"follow up","model":"gpt-5.6-luna","effort":"high","continuation":{"type":"previous_task","task_id":source_id}});
        let successor = task_start_with_store_and_binary(&args, &owner, &store, &binary)
            .await
            .unwrap();
        let successor_id = Uuid::parse_str(successor["task_id"].as_str().unwrap()).unwrap();
        assert_ne!(source_id, successor_id);
        assert_eq!(successor["continued_from_task_id"], json!(source_id));
        assert_eq!(successor["verification"]["status"], "not_run");
        assert_eq!(successor["delivery"]["status"], "not_started");
        let retained_source = store.load(&owner, source_id).unwrap();
        assert_eq!(retained_source.status, TaskStatus::Completed);
        assert_eq!(
            retained_source.thread_id,
            Some("0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa".to_owned())
        );
        assert_eq!(
            retained_source.turn_id,
            Some("0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb".to_owned())
        );
        assert_eq!(retained_source.continued_by_task_id, Some(successor_id));
        assert!(runtime_for(&owner, source_id).is_none());
        let methods = std::fs::read_to_string(state.path().join("methods.log")).unwrap();
        assert_eq!(methods.matches("thread/start\n").count(), 1);
        assert_eq!(methods.matches("thread/resume\n").count(), 1);
        assert_eq!(methods.matches("turn/start\n").count(), 2);
        let replay = task_start_with_store_and_binary(&args, &owner, &store, &binary)
            .await
            .unwrap();
        assert_eq!(replay["task_id"], successor["task_id"]);
        assert_eq!(
            std::fs::read_to_string(state.path().join("methods.log")).unwrap(),
            methods
        );
        assert!(
            store
                .accept_control(&owner, source_id, Uuid::new_v4(), Uuid::new_v4(), "steer")
                .unwrap_err()
                .to_string()
                .contains("CODEX_CONTINUATION_CLAIMED")
        );
        assert!(
            store
                .accept_control(&owner, source_id, Uuid::new_v4(), Uuid::new_v4(), "resume")
                .unwrap_err()
                .to_string()
                .contains("CODEX_CONTINUATION_CLAIMED")
        );
        shutdown_session_runtimes(&SessionInstance::from_session(&owner)).await;
    }

    #[tokio::test]
    async fn crash_after_source_claim_allows_only_exact_successor_retry() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let owner = session(workspace.path(), "continuation-crash", true);
        let store = TaskStore::new(state.path().join("tasks"));
        let binary = fake_app_server(state.path(), "continuation");
        let source_id = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let successor_id = task_id_for_operation(&owner, operation_id).unwrap();
        let args = json!({"operation_id":operation_id,"task":"follow up","model":"gpt-5.6-luna","effort":"high","continuation":{"type":"previous_task","task_id":source_id}});
        let fingerprint = continuation_fingerprint(
            successor_id,
            "follow up",
            "gpt-5.6-luna",
            "high",
            &TaskStartOrigin::Generic,
            CodexContinuation::PreviousTask { task_id: source_id },
        )
        .unwrap();
        let mut source = task_record(
            &owner,
            source_id,
            TaskStatus::Completed,
            2,
            Some("0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa"),
            Some("old-turn"),
        );
        source.continued_by_task_id = Some(successor_id);
        source.continued_by_request_fingerprint = Some(fingerprint);
        store.save(&source).unwrap();
        assert!(
            !store.path(successor_id).exists(),
            "fault point: source claim persisted before successor record"
        );
        let changed = json!({"operation_id":operation_id,"task":"different","model":"gpt-5.6-luna","effort":"high","continuation":{"type":"previous_task","task_id":source_id}});
        assert!(
            task_start_with_store_and_binary(&changed, &owner, &store, &binary)
                .await
                .unwrap_err()
                .to_string()
                .contains("OPERATION_CONFLICT")
        );
        let other_source_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                other_source_id,
                TaskStatus::Completed,
                2,
                Some("another-thread"),
                Some("another-turn"),
            ))
            .unwrap();
        let redirected = json!({"operation_id":operation_id,"task":"follow up","model":"gpt-5.6-luna","effort":"high","continuation":{"type":"previous_task","task_id":other_source_id}});
        assert!(
            task_start_with_store_and_binary(&redirected, &owner, &store, &binary)
                .await
                .unwrap_err()
                .to_string()
                .contains("OPERATION_CONFLICT")
        );
        let fresh_thread = json!({"operation_id":operation_id,"task":"follow up","model":"gpt-5.6-luna","effort":"high"});
        assert!(
            task_start_with_store_and_binary(&fresh_thread, &owner, &store, &binary)
                .await
                .unwrap_err()
                .to_string()
                .contains("OPERATION_CONFLICT")
        );
        let view = task_start_with_store_and_binary(&args, &owner, &store, &binary)
            .await
            .unwrap();
        assert_eq!(view["task_id"], json!(successor_id));
        assert_eq!(
            store.load(&owner, source_id).unwrap().continued_by_task_id,
            Some(successor_id)
        );
        let other = json!({"operation_id":Uuid::new_v4(),"task":"racer","model":"gpt-5.6-luna","effort":"high","continuation":{"type":"previous_task","task_id":source_id}});
        assert!(
            task_start_with_store_and_binary(&other, &owner, &store, &binary)
                .await
                .unwrap_err()
                .to_string()
                .contains("CODEX_CONTINUATION_CLAIMED")
        );
        let methods = std::fs::read_to_string(state.path().join("methods.log")).unwrap();
        assert_eq!(methods.matches("thread/resume\n").count(), 1);
        assert_eq!(methods.matches("thread/start\n").count(), 0);
        shutdown_session_runtimes(&SessionInstance::from_session(&owner)).await;
    }

    #[tokio::test]
    async fn racing_successors_claim_one_conversation() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let owner = session(workspace.path(), "continuation-race", true);
        let store = TaskStore::new(state.path().join("tasks"));
        let binary = fake_app_server(state.path(), "continuation");
        let source_id = Uuid::new_v4();
        store
            .save(&task_record(
                &owner,
                source_id,
                TaskStatus::Completed,
                2,
                Some("0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa"),
                Some("old-turn"),
            ))
            .unwrap();
        let args_b = json!({"operation_id":Uuid::new_v4(),"task":"B","model":"gpt-5.6-luna","effort":"high","continuation":{"type":"previous_task","task_id":source_id}});
        let args_c = json!({"operation_id":Uuid::new_v4(),"task":"C","model":"gpt-5.6-luna","effort":"high","continuation":{"type":"previous_task","task_id":source_id}});
        let (b, c) = tokio::join!(
            task_start_with_store_and_binary(&args_b, &owner, &store, &binary),
            task_start_with_store_and_binary(&args_c, &owner, &store, &binary)
        );
        assert_eq!(usize::from(b.is_ok()) + usize::from(c.is_ok()), 1);
        let winner = b.as_ref().ok().or_else(|| c.as_ref().ok()).unwrap();
        assert_eq!(
            store.load(&owner, source_id).unwrap().continued_by_task_id,
            Some(Uuid::parse_str(winner["task_id"].as_str().unwrap()).unwrap())
        );
        let methods = std::fs::read_to_string(state.path().join("methods.log")).unwrap();
        assert_eq!(methods.matches("thread/resume\n").count(), 1);
        assert_eq!(methods.matches("turn/start\n").count(), 1);
        shutdown_session_runtimes(&SessionInstance::from_session(&owner)).await;
    }

    #[tokio::test]
    async fn claimed_source_get_is_read_only_without_runtime_reconstruction() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (approval_sender, _approval_receiver) = approvals::approval_channel();
        let handle = approvals::spawn_runtime(
            workspace.path(),
            Some("claimed-source-get"),
            true,
            approval_sender,
        )
        .await
        .unwrap();
        let owner = config::read_session_metadata("claimed-source-get")
            .await
            .unwrap();
        let store = TaskStore::new(state.path().join("tasks"));
        let source_id = Uuid::new_v4();
        let mut source = task_record(
            &owner,
            source_id,
            TaskStatus::Completed,
            2,
            Some("retained-thread"),
            Some("retained-turn"),
        );
        source.continued_by_task_id = Some(Uuid::new_v4());
        source.continued_by_request_fingerprint = Some(Uuid::new_v4());
        store.save(&source).unwrap();
        let view = task_get_with_store_and_binary(
            &json!({"task_id":source_id}),
            &owner,
            &store,
            &state.path().join("missing-codex"),
        )
        .await
        .unwrap();
        assert_eq!(view["status"], "completed");
        assert_eq!(view["thread_id"], "retained-thread");
        assert!(runtime_for(&owner, source_id).is_none());
        remove_session(&owner).await.unwrap();
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn continuation_rejects_unavailable_scope_and_nonquiescent_sources() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let owner = session(workspace.path(), "continuation-boundary", true);
        let store = TaskStore::new(state.path().join("tasks"));
        let missing_binary = state.path().join("must-not-start");
        let source_id = Uuid::new_v4();
        let args = json!({"operation_id":Uuid::new_v4(),"task":"follow up","model":"gpt-5.6-luna","effort":"high","continuation":{"type":"previous_task","task_id":source_id}});
        assert!(
            task_start_with_store_and_binary(&args, &owner, &store, &missing_binary)
                .await
                .unwrap_err()
                .to_string()
                .contains("CODEX_CONTINUATION_SOURCE_UNAVAILABLE")
        );
        let mut source = task_record(
            &owner,
            source_id,
            TaskStatus::Running,
            1,
            Some("thread"),
            Some("turn"),
        );
        store.save(&source).unwrap();
        assert!(
            task_start_with_store_and_binary(&args, &owner, &store, &missing_binary)
                .await
                .unwrap_err()
                .to_string()
                .contains("CODEX_CONTINUATION_NOT_QUIESCENT")
        );
        source.status = TaskStatus::Completed;
        source.thread_id = None;
        store.save(&source).unwrap();
        assert!(
            task_start_with_store_and_binary(&args, &owner, &store, &missing_binary)
                .await
                .unwrap_err()
                .to_string()
                .contains("CODEX_CONTINUATION_UNAVAILABLE")
        );
        source.thread_id = Some("thread".to_owned());
        source.operations.push(OperationReceipt {
            operation_id: Uuid::new_v4(),
            request_fingerprint: Uuid::new_v4(),
            action: "steer".to_owned(),
            phase: OperationPhase::Accepted,
            outcome: source.outcome(),
        });
        store.save(&source).unwrap();
        assert!(
            task_start_with_store_and_binary(&args, &owner, &store, &missing_binary)
                .await
                .unwrap_err()
                .to_string()
                .contains("CODEX_CONTINUATION_NOT_QUIESCENT")
        );
        source.operations.clear();
        store.save(&source).unwrap();
        let other_session = session(workspace.path(), "other-instance", true);
        assert!(
            task_start_with_store_and_binary(&args, &other_session, &store, &missing_binary)
                .await
                .unwrap_err()
                .to_string()
                .contains("CODEX_CONTINUATION_SOURCE_UNAVAILABLE")
        );
    }

    #[test]
    fn generated_crash_claim_acceptance_matches_single_successor_model() -> noprop::TestResult {
        crate::test_support::run(0x4343_315f_434c_4149, 128, |ctx| {
            let workspace = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let owner = session(workspace.path(), "claim-property", true);
            let store = TaskStore::new(state.path().join("tasks"));
            let source_id = Uuid::new_v4();
            let winner_id = Uuid::new_v4();
            let winner_fingerprint = Uuid::new_v4();
            let case = noprop::sample_u8(ctx) % 3;
            let in_flight = noprop::sample_u8(ctx).is_multiple_of(2);
            let mut source = task_record(
                &owner,
                source_id,
                TaskStatus::Completed,
                2,
                Some("thread"),
                Some("turn"),
            );
            source.continued_by_task_id = Some(winner_id);
            source.continued_by_request_fingerprint = Some(winner_fingerprint);
            if in_flight {
                source.operations.push(OperationReceipt {
                    operation_id: Uuid::new_v4(),
                    request_fingerprint: Uuid::new_v4(),
                    action: "control".to_owned(),
                    phase: OperationPhase::Accepted,
                    outcome: source.outcome(),
                });
            }
            store.save(&source).unwrap();
            let candidate_id = if case == 2 { Uuid::new_v4() } else { winner_id };
            let candidate_fingerprint = if case == 1 {
                Uuid::new_v4()
            } else {
                winner_fingerprint
            };
            let mut candidate =
                task_record(&owner, candidate_id, TaskStatus::Accepted, 1, None, None);
            candidate.continued_from_task_id = Some(source_id);
            candidate.operations.push(OperationReceipt {
                operation_id: Uuid::new_v4(),
                request_fingerprint: candidate_fingerprint,
                action: "start".to_owned(),
                phase: OperationPhase::Accepted,
                outcome: candidate.outcome(),
            });
            let accepted = store.accept_start(&owner, candidate).is_ok();
            assert_eq!(accepted, !in_flight && case == 0);
            assert_eq!(
                store.load(&owner, source_id).unwrap().continued_by_task_id,
                Some(winner_id)
            );
            Ok(())
        })
    }
}
