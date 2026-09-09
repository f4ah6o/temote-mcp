use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{approvals, config, evidence};

const SUPPORTED_APP_SERVER_VERSION: &str = "0.153.4";
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
    cancellation: watch::Sender<bool>,
}

impl CodexLifecycleEntry {
    fn new() -> Self {
        let (cancellation, _) = watch::channel(false);
        Self {
            closing: false,
            cleanup_complete: false,
            in_flight: 0,
            cancellation,
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

fn finish_session_shutdown(owner: &SessionInstance) {
    let mut registry = codex_lifecycle_registry().lock().unwrap();
    let should_remove = registry.entries.get_mut(owner).is_some_and(|entry| {
        entry.cleanup_complete = true;
        entry.in_flight == 0
    });
    if should_remove {
        registry.entries.remove(owner);
    }
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
        let mut registry = codex_lifecycle_registry().lock().unwrap();
        let should_remove = registry.entries.get_mut(&self.owner).is_some_and(|entry| {
            entry.in_flight = entry.in_flight.saturating_sub(1);
            entry.cleanup_complete && entry.in_flight == 0
        });
        if should_remove {
            registry.entries.remove(&self.owner);
        }
    }
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
    turn_id: Option<String>,
    #[serde(default)]
    usage: Option<BTreeMap<String, u64>>,
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
    Accepted(TaskRecord),
}

#[derive(Debug)]
enum ControlAcceptance {
    Replay(Value),
    Accepted(Box<TaskRecord>),
}

fn store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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
                std::fs::create_dir(&self.directory)?;
                #[cfg(unix)]
                std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))?;
                let metadata = std::fs::symlink_metadata(&self.directory)?;
                validate_store_directory(&self.directory, &metadata)
            }
            Err(error) => Err(error).context("cannot inspect Codex task store"),
        }
    }

    fn path(&self, task_id: Uuid) -> PathBuf {
        self.directory.join(format!("{task_id}.json"))
    }

    fn load(&self, session: &config::Session, task_id: Uuid) -> Result<TaskRecord> {
        let _guard = store_lock().lock().unwrap();
        self.load_locked(session, task_id)
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
        let _guard = store_lock().lock().unwrap();
        self.save_locked(record)
    }

    fn save_locked(&self, record: &TaskRecord) -> Result<()> {
        self.ensure_directory()?;
        validate_record(record)?;
        self.prune_locked(record)?;
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
        let _guard = store_lock().lock().unwrap();
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
        let _guard = store_lock().lock().unwrap();
        let registry = codex_lifecycle_registry().lock().unwrap();
        let entry = registry
            .entries
            .get(owner)
            .context("Codex session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "Codex session instance is closing");
        let mut record = self.load_locked(session, task_id)?;
        f(&mut record)?;
        record.updated_at = config::unix_time();
        self.save_locked(&record)?;
        Ok(record)
    }

    fn accept_start(
        &self,
        session: &config::Session,
        record: TaskRecord,
    ) -> Result<StartAcceptance> {
        let _guard = store_lock().lock().unwrap();
        self.accept_start_locked(session, record)
    }

    fn accept_start_if_instance_live(
        &self,
        session: &config::Session,
        record: TaskRecord,
        owner: &SessionInstance,
    ) -> Result<StartAcceptance> {
        let _guard = store_lock().lock().unwrap();
        let registry = codex_lifecycle_registry().lock().unwrap();
        let entry = registry
            .entries
            .get(owner)
            .context("Codex session instance lifecycle state is unavailable")?;
        anyhow::ensure!(!entry.closing, "Codex session instance is closing");
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
                    existing.status = TaskStatus::Accepted;
                    existing.revision = existing.revision.saturating_add(1);
                    update_operation_receipt(
                        &mut existing,
                        candidate_receipt.operation_id,
                        OperationPhase::Accepted,
                    );
                    existing.updated_at = config::unix_time();
                    self.save_locked(&existing)?;
                    Ok(StartAcceptance::Accepted(existing))
                } else {
                    Ok(StartAcceptance::Existing(existing))
                }
            }
            Err(error) if is_not_found(&error) => {
                self.save_locked(&record)?;
                Ok(StartAcceptance::Accepted(record))
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
        let _guard = store_lock().lock().unwrap();
        self.accept_control_locked(session, task_id, operation_id, request_fingerprint, action)
    }

    fn accept_control_if_instance_live(
        &self,
        session: &config::Session,
        task_id: Uuid,
        operation_id: Uuid,
        request_fingerprint: Uuid,
        action: &str,
        owner: &SessionInstance,
    ) -> Result<ControlAcceptance> {
        let _guard = store_lock().lock().unwrap();
        let registry = codex_lifecycle_registry().lock().unwrap();
        let entry = registry
            .entries
            .get(owner)
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
                    TaskStatus::Unknown | TaskStatus::ReconciliationRequired
                ),
                "Codex task does not require resume reconciliation"
            );
        } else {
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
        Ok(ControlAcceptance::Accepted(Box::new(record)))
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
            let runtime_backed = runtime_matches_record(&record);
            let terminal = record.status.is_terminal();
            if expired && terminal && !runtime_backed && id != current.task_id {
                std::fs::remove_file(entry.path())
                    .with_context(|| format!("cannot prune expired Codex task {id}"))?;
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

    fn finalize_owner(&self, owner: &SessionInstance) -> Result<usize> {
        let _guard = store_lock().lock().unwrap();
        let metadata = match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error).context("cannot inspect Codex task store"),
        };
        validate_store_directory(&self.directory, &metadata)?;
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error).context("cannot list Codex task store"),
        };
        let now = config::unix_time();
        let mut count = 0usize;
        let mut finalized = 0usize;
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
            if record.owner != owner.clone() || record.status.is_terminal() {
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
        Ok(finalized)
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

fn task_view(record: &TaskRecord, evidence_ref: Option<&evidence::EvidenceRef>) -> Value {
    json!({
        "task_id": record.task_id,
        "status": record.status.as_str(),
        "revision": record.revision,
        "generation": record.generation,
        "model": record.model,
        "effort": record.effort,
        "thread_id": record.thread_id,
        "turn_id": record.turn_id,
        "usage": record.usage,
        "reconciliation_required": record.status == TaskStatus::ReconciliationRequired,
        "evidence": evidence_ref,
        "retention_seconds": TASK_RETENTION_SECONDS,
    })
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
    evidence::store(
        &session.id,
        &session.cwd,
        serde_json::to_string(response).ok()?,
    )
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
        let _ = self.tx.send(ClientCommand::Shutdown).await;
        let actor = self.actor.lock().unwrap().take();
        if let Some(actor) = actor {
            let _ = actor.await;
        }
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
    started_at: Instant,
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

async fn insert_runtime(session: &config::Session, task_id: Uuid, client: RpcClient) -> Result<()> {
    let owner = SessionInstance::from_session(session);
    let _permit = ensure_current_active_instance(&owner, session).await?;
    insert_runtime_unchecked(session, task_id, client, &owner)
}

fn insert_runtime_unchecked(
    session: &config::Session,
    task_id: Uuid,
    client: RpcClient,
    owner: &SessionInstance,
) -> Result<()> {
    let owner = owner.clone();
    let scope = config::canonical_directory(&session.cwd)?;
    let runtime = RuntimeHandle {
        client: client.clone(),
        owner: owner.clone(),
        scope,
        started_at: Instant::now(),
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
        if session_stopped && !active_session_exists(&owner.id).await {
            evidence::remove_session(&owner.id);
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
    let result = store
        .finalize_owner(owner)
        .context("failed to finalize Codex tasks for ended session instance")
        .map(|_| ());
    remove_session_evidence(owner).await;
    if result.is_ok() {
        finish_session_shutdown(owner);
    }
    result
}

#[cfg(test)]
async fn remove_session_with_store(session: &config::Session, store: &TaskStore) -> Result<()> {
    let owner = SessionInstance::from_session(session);
    begin_session_instance_shutdown(&owner);
    shutdown_session_runtimes(&owner).await;
    finalize_session_tasks(&owner, store).await
}

pub(crate) async fn remove_session(session: &config::Session) -> Result<()> {
    let owner = SessionInstance::from_session(session);
    begin_session_instance_shutdown(&owner);
    shutdown_session_runtimes(&owner).await;
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
    spawn_initialized_client_with_binary(session, task_id, Path::new("codex")).await
}

async fn spawn_initialized_client_with_binary(
    session: &config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
) -> Result<(RpcClient, Value)> {
    spawn_initialized_client_with_binary_mode(session, task_id, binary, true).await
}

async fn spawn_initialized_client_with_binary_mode(
    session: &config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
    fence: bool,
) -> Result<(RpcClient, Value)> {
    let owner = SessionInstance::from_session(session);
    let spawn_permit = if fence {
        Some(ensure_current_active_instance(&owner, session).await?)
    } else {
        None
    };
    let client = if fence {
        let client = spawn_client_if_instance_live(session.clone(), task_id, binary, &owner)?;
        drop(spawn_permit);
        client
    } else {
        spawn_client_with_binary_unchecked(session.clone(), task_id, binary)?
    };
    let initialized = async {
        let initialize_params = json!({
            "clientInfo": {
                "name": "temote-mcp",
                "version": env!("CARGO_PKG_VERSION")
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
        Ok(initialized) => Ok((client, initialized)),
        Err(error) => {
            client.shutdown().await;
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
        user_agent.split_whitespace().next().is_some_and(|prefix| {
            prefix == format!("codex_cli_rs/{SUPPORTED_APP_SERVER_VERSION}")
        }),
        "CODEX_APP_SERVER_INCOMPATIBLE: expected {SUPPORTED_APP_SERVER_VERSION}, got {user_agent}"
    );
    anyhow::ensure!(
        object.get("codexHome").and_then(Value::as_str).is_some(),
        "Codex app-server initialize result is missing codexHome"
    );
    Ok(())
}

fn spawn_client_if_instance_live(
    session: config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
    owner: &SessionInstance,
) -> Result<RpcClient> {
    let registry = codex_lifecycle_registry().lock().unwrap();
    let entry = registry
        .entries
        .get(owner)
        .context("Codex session instance lifecycle state is unavailable")?;
    anyhow::ensure!(!entry.closing, "Codex session instance is closing");
    spawn_client_with_binary_unchecked(session, task_id, binary)
}

fn spawn_client_with_binary_unchecked(
    session: config::Session,
    task_id: Option<Uuid>,
    binary: &Path,
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
    let actor = tokio::spawn(run_actor(child, stdin, stdout, session, task_id, rx));
    Ok(RpcClient {
        tx,
        actor: Arc::new(Mutex::new(Some(actor))),
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

async fn run_actor(
    mut child: tokio::process::Child,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    session: config::Session,
    task_id: Option<Uuid>,
    mut commands: mpsc::Receiver<ClientCommand>,
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
            line = read_bounded_json_line(&mut reader) => {
                match line {
                    Ok(Some(value)) => {
                        if let Some(method) = value.get("method").and_then(Value::as_str) {
                            let method = method.to_owned();
                            if let Some(id) = value.get("id").cloned() {
                                let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
                                let tx = server_tx.clone();
                                let session = session.clone();
                                tokio::spawn(async move {
                                    let payload = handle_server_request(&session, task_id, &method, params).await;
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
    let _ = child.kill().await;
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
    let value = serde_json::from_slice(&line).context("invalid Codex app-server JSON line")?;
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
        let mut changed = false;
        if let Some(turn_id) = turn_id.as_deref()
            && record.turn_id.as_deref() != Some(turn_id)
        {
            record.turn_id = Some(turn_id.to_owned());
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
) -> std::result::Result<Value, (i64, String)> {
    match method {
        "item/commandExecution/requestApproval" => {
            let task_id = task_id.ok_or_else(|| {
                (
                    -32601,
                    "approval request is unavailable outside a Codex task".to_owned(),
                )
            })?;
            let owner = SessionInstance::from_session(session);
            if ensure_current_active_instance(&owner, session)
                .await
                .is_err()
            {
                return Ok(json!({"decision": "decline"}));
            }
            validate_approval_task(session, task_id, &params)?;
            mark_waiting_approval(session, task_id, true);
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
            Ok(json!({"decision": if allowed { "accept" } else { "decline" }}))
        }
        "item/fileChange/requestApproval" => {
            let task_id = task_id.ok_or_else(|| {
                (
                    -32601,
                    "approval request is unavailable outside a Codex task".to_owned(),
                )
            })?;
            let owner = SessionInstance::from_session(session);
            if ensure_current_active_instance(&owner, session)
                .await
                .is_err()
            {
                return Ok(json!({"decision": "decline"}));
            }
            validate_approval_task(session, task_id, &params)?;
            mark_waiting_approval(session, task_id, true);
            let (detail, metadata) =
                child_approval("file change", "fileChange", "file_change", task_id, &params);
            let allowed =
                request_child_approval(session, "Codex file-change approval", detail, metadata)
                    .await;
            mark_waiting_approval(session, task_id, false);
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
) -> std::result::Result<(), (i64, String)> {
    let store = TaskStore::default_store().map_err(internal_server_error)?;
    bind_approval_turn(&store, session, task_id, params)
        .map_err(|error| (-32602, error.to_string()))
}

fn bind_approval_turn(
    store: &TaskStore,
    session: &config::Session,
    task_id: Uuid,
    params: &Value,
) -> Result<()> {
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .context("approval request is missing threadId")?;
    let turn_id = params
        .get("turnId")
        .and_then(Value::as_str)
        .context("approval request is missing turnId")?;
    store.update(session, task_id, |record| {
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
    Ok(())
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
        efforts.iter().any(|entry| {
            entry.get("effort").and_then(Value::as_str) == Some(effort)
                || entry.as_str() == Some(effort)
        }),
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
    let advertised = data
        .iter()
        .filter_map(|entry| {
            let model = entry.get("model")?.as_str()?;
            let efforts = entry
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            item.get("effort")
                                .and_then(Value::as_str)
                                .or_else(|| item.as_str())
                                .map(str::to_owned)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(json!({"model": model, "efforts": efforts}))
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "compatible": true,
        "app_server_version": SUPPORTED_APP_SERVER_VERSION,
        "platform_family": initialized.get("platformFamily"),
        "platform_os": initialized.get("platformOs"),
        "models": advertised,
    }))
}

pub(crate) async fn task_start(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_start_with_store_and_binary_inner(args, session, &store, Path::new("codex"), true).await
}

#[cfg(test)]
async fn task_start_with_store_and_binary(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
) -> Result<Value> {
    task_start_with_store_and_binary_inner(args, session, store, binary, false).await
}

#[cfg(test)]
async fn task_start_with_store_and_binary_fenced(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
) -> Result<Value> {
    task_start_with_store_and_binary_inner(args, session, store, binary, true).await
}

async fn task_start_with_store_and_binary_inner(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
    binary: &Path,
    fence: bool,
) -> Result<Value> {
    let owner = SessionInstance::from_session(session);
    let operation_id = required_uuid(args, "operation_id")?;
    let task = required_string(args, "task")?;
    let model = required_string(args, "model")?;
    let effort = required_string(args, "effort")?;
    validate_task_input(task, "task")?;
    validate_argument(model, "model")?;
    validate_argument(effort, "effort")?;

    let task_id = task_id_for_operation(session, operation_id)?;
    let request_fingerprint = fingerprint(&json!({
        "kind": "start",
        "task_id": task_id,
        "task": task,
        "model": model,
        "effort": effort,
    }))?;
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
        turn_id: None,
        usage: None,
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
    let mut record = match acceptance {
        StartAcceptance::Existing(existing) => {
            return replay_operation(&existing, operation_id, request_fingerprint);
        }
        StartAcceptance::Accepted(record) => record,
    };

    let client_result =
        spawn_initialized_client_with_binary_mode(session, Some(task_id), binary, fence).await;
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
        let thread = request_codex(
            &client,
            &owner,
            session,
            "thread/start",
            json!({
                "cwd": cwd,
                "model": model,
                "approvalPolicy": CODEX_APPROVAL_POLICY,
                "approvalsReviewer": "user",
                "sandbox": "workspaceWrite",
                "runtimeWorkspaceRoots": [record.scope_cwd],
                "ephemeral": false,
                "threadSource": "temote-mcp",
            }),
            fence,
        )
        .await?;
        let thread_id = thread
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .context("thread/start response is missing thread.id")?
            .to_owned();
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

    if let Err(_error) = start_result {
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
        return Ok(task_view(&record, None));
    }

    let insert_result = if fence {
        insert_runtime(session, task_id, client.clone()).await
    } else {
        insert_runtime_unchecked(session, task_id, client.clone(), &owner)
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

pub(crate) async fn task_get(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_get_with_store_and_binary(args, session, &store, Path::new("codex")).await
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
    let load_permit = ensure_current_active_instance(&owner, session).await?;
    let mut record = store.load(session, task_id)?;
    drop(load_permit);
    if record.thread_id.is_none() {
        if record.status == TaskStatus::Accepted {
            let start_operation_id = record
                .operations
                .iter()
                .find(|receipt| receipt.action == "start")
                .map(|receipt| receipt.operation_id);
            let apply_permit = ensure_current_active_instance(&owner, session).await?;
            record = store.update_if_instance_live(session, task_id, &owner, |record| {
                if record.status.is_terminal() {
                    return Ok(());
                }
                record.status = TaskStatus::ReconciliationRequired;
                record.revision = record.revision.saturating_add(1);
                if let Some(operation_id) = start_operation_id {
                    update_operation_receipt(record, operation_id, OperationPhase::Accepted);
                }
                Ok(())
            })?;
            drop(apply_permit);
        }
        return Ok(task_view(&record, None));
    }

    let client = match ensure_runtime_with_binary(session, &record, binary).await {
        Ok(client) => client,
        Err(_) => {
            let apply_permit = ensure_current_active_instance(&owner, session).await?;
            record = store.update_if_instance_live(session, task_id, &owner, |record| {
                if !record.status.is_terminal() {
                    record.status = TaskStatus::Unknown;
                    record.revision = record.revision.saturating_add(1);
                }
                Ok(())
            })?;
            drop(apply_permit);
            return Ok(task_view(&record, None));
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
            let record = store.update_if_instance_live(session, task_id, &owner, |record| {
                if !record.status.is_terminal() {
                    record.status = TaskStatus::Unknown;
                    record.revision = record.revision.saturating_add(1);
                }
                Ok(())
            })?;
            drop(apply_permit);
            return Ok(task_view(&record, None));
        }
    };
    let evidence_permit = ensure_current_active_instance(&owner, session).await?;
    let evidence_ref = store_evidence_for_instance(&owner, session, &response);
    drop(evidence_permit);
    let derived = match derive_thread_state(&response, record.turn_id.as_deref()) {
        Ok(derived) => derived,
        Err(_) => {
            let apply_permit = ensure_current_active_instance(&owner, session).await?;
            record = store.update_if_instance_live(session, task_id, &owner, |record| {
                if !record.status.is_terminal() {
                    record.status = TaskStatus::Unknown;
                    record.revision = record.revision.saturating_add(1);
                }
                Ok(())
            })?;
            drop(apply_permit);
            return Ok(task_view(&record, evidence_ref.as_ref()));
        }
    };
    let reconciled_status = reconciled_task_status(record.status, derived.status);
    let changed = record.status != reconciled_status
        || record.turn_id != derived.turn_id
        || (derived.usage.is_some() && record.usage != derived.usage);
    if changed || (record.generation == 0 && derived.turn_id.is_some()) {
        let apply_permit = ensure_current_active_instance(&owner, session).await?;
        record = store.update_if_instance_live(session, task_id, &owner, |record| {
            if record.status.is_terminal() {
                return Ok(());
            }
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
            record.revision = record.revision.saturating_add(1);
            let outcome = record.outcome();
            if derived.turn_id.is_some()
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
    }
    if after_revision == Some(record.revision) {
        return Ok(json!({
            "task_id": task_id,
            "status": "not_modified",
            "revision": record.revision,
        }));
    }
    Ok(task_view(&record, evidence_ref.as_ref()))
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
}

fn derive_thread_state(
    response: &Value,
    expected_turn_id: Option<&str>,
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
        .or_else(|| turns.last());
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
                "idle" => TaskStatus::Unknown,
                "active" => TaskStatus::Running,
                "systemError" => TaskStatus::Failed,
                _ => TaskStatus::Unknown,
            },
            turn_id: None,
            usage,
        });
    };
    let turn_id = turn.get("id").and_then(Value::as_str).map(str::to_owned);
    let status = turn
        .get("status")
        .and_then(Value::as_str)
        .and_then(task_status_from_str)
        .unwrap_or(TaskStatus::Unknown);
    Ok(DerivedThreadState {
        status,
        turn_id,
        usage,
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

async fn ensure_runtime_with_binary(
    session: &config::Session,
    record: &TaskRecord,
    binary: &Path,
) -> Result<RpcClient> {
    let owner = SessionInstance::from_session(session);
    if let Some(runtime) = runtime_for(session, record.task_id) {
        return Ok(runtime.client);
    }
    let (client, _) =
        spawn_initialized_client_with_binary(session, Some(record.task_id), binary).await?;
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
            "sandbox": "workspaceWrite",
            "runtimeWorkspaceRoots": [record.scope_cwd],
            "excludeTurns": true,
        }),
    )
    .await;
    if let Err(error) = resume {
        client.shutdown().await;
        return Err(error).context("Codex task could not resume its retained thread");
    }
    if let Err(error) = insert_runtime(session, record.task_id, client.clone()).await {
        client.shutdown().await;
        return Err(error);
    }
    Ok(client)
}

pub(crate) async fn task_control(args: &Value, session: &config::Session) -> Result<Value> {
    let store = TaskStore::default_store()?;
    task_control_with_store_and_binary(args, session, &store, Path::new("codex")).await
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
    let mut record = match store.accept_control_if_instance_live(
        session,
        task_id,
        operation_id,
        request_fingerprint,
        action,
        &owner,
    )? {
        ControlAcceptance::Replay(result) => return Ok(result),
        ControlAcceptance::Accepted(record) => *record,
    };
    drop(acceptance_permit);
    let thread_id = record
        .thread_id
        .clone()
        .context("Codex task requires reconciliation before control")?;
    let turn_id = record.turn_id.clone();

    let client = match ensure_runtime_with_binary(session, &record, binary).await {
        Ok(client) => client,
        Err(_) => {
            let shutting_down = session_instance_is_closing(&owner);
            let record =
                apply_control_failure(store, session, task_id, operation_id, shutting_down)?;
            return Ok(task_view(&record, None));
        }
    };
    let result = match action {
        "steer" => {
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
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Barrier, mpsc};

    fn session(root: &Path, id: &str, yolo: bool) -> config::Session {
        let cwd = config::canonical_directory(root).unwrap();
        config::Session {
            id: id.to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1234,
            process_id: 5678,
            yolo,
        }
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
    if method == 'initialize':
        result = {'userAgent':'codex_cli_rs/0.153.4 (temote-mcp; test)','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'macos'}
    elif method == 'model/list':
        if mode == 'model-fail':
            print(json.dumps({'id':i,'error':{'code':-1,'message':'model list unavailable'}}), flush=True)
            continue
        model = 'other-model' if mode == 'invalid-model' else 'gpt-5.6-luna'
        result = {'data':[{'model':model,'id':'luna','displayName':'Luna','description':'test','hidden':False,'isDefault':True,'defaultReasoningEffort':'high','supportedReasoningEfforts':[{'effort':'low'},{'effort':'medium'},{'effort':'high'},{'effort':'max'},{'effort':'xhigh'}]}]}
    elif method == 'thread/start':
        if mode == 'thread-uncertain':
            print(json.dumps({'id':i,'error':{'code':-1,'message':'thread start response lost'}}), flush=True)
            continue
        if mode == 'reject-never' and req.get('params',{}).get('approvalPolicy') == 'never':
            print(json.dumps({'id':i,'error':{'code':-1,'message':'never approval policy rejected'}}), flush=True)
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
        result = {'thread':{'id':thread_id}}
    elif method == 'thread/read':
        result = {'thread':{'id':thread_id,'status':{'type':'idle'},'tokenUsage':{'inputTokens':4,'cachedInputTokens':1,'outputTokens':2,'reasoningOutputTokens':1,'totalTokens':6},'turns':[{'id':turn_id,'status':'completed','items':[{'type':'agentMessage','id':'m','text':'secret transcript marker'}]}]}}
    elif method == 'turn/steer':
        result = {'turnId':turn_id}
    elif method == 'turn/interrupt':
        result = {}
    else:
        print(json.dumps({'id':i,'error':{'code':-32601,'message':'unsupported'}}), flush=True)
        continue
    print(json.dumps({'id':i,'result':result}), flush=True)
"##;
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
        result = {'userAgent':'codex_cli_rs/0.153.4 (temote-mcp; test)','codexHome':'/tmp/codex','platformFamily':'unix','platformOs':'macos'}
    elif method == 'model/list':
        result = {'data':[{'model':'gpt-5.6-luna','id':'luna','displayName':'Luna','description':'test','hidden':False,'isDefault':True,'defaultReasoningEffort':'high','supportedReasoningEfforts':[{'effort':'low'},{'effort':'medium'},{'effort':'high'},{'effort':'max'},{'effort':'xhigh'}]}]}
    elif method == blocked_method:
        open(entered, 'w').close()
        while not os.path.exists(release):
            time.sleep(0.005)
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
        .replace("__STEER_SENT__", &python_string(&steer_sent));
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

    async fn wait_for_marker(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fake app-server did not reach its barrier");
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
            turn_id: turn_id.map(str::to_owned),
            usage: None,
            created_at: now,
            updated_at: now,
            operations: Vec::new(),
            operation_tombstones: Vec::new(),
        }
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
            turn_id: None,
            usage: None,
            created_at: now,
            updated_at: now,
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
            turn_id: None,
            usage: None,
            created_at: now,
            updated_at: now,
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
        };

        let (replacement_commands, mut replacement_receiver) = tokio::sync::mpsc::channel(1);
        let replacement_actor = tokio::spawn(async move {
            let _ = replacement_receiver.recv().await;
        });
        let replacement_client = RpcClient {
            tx: replacement_commands,
            actor: Arc::new(Mutex::new(Some(replacement_actor))),
        };

        runtimes().lock().unwrap().insert(
            old_task,
            RuntimeHandle {
                client: old_client,
                owner: SessionInstance::from_session(&old),
                scope: old.cwd.clone(),
                started_at: Instant::now(),
            },
        );
        runtimes().lock().unwrap().insert(
            replacement_task,
            RuntimeHandle {
                client: replacement_client,
                owner: SessionInstance::from_session(&replacement),
                scope: replacement.cwd.clone(),
                started_at: Instant::now(),
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
        let error = insert_runtime(&owner, task_id, client.clone())
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
        runtimes().lock().unwrap().insert(
            task_id,
            RuntimeHandle {
                client: RpcClient {
                    tx: commands,
                    actor: Arc::new(Mutex::new(Some(actor))),
                },
                owner: SessionInstance::from_session(&owner),
                scope: owner.cwd.clone(),
                started_at: Instant::now(),
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
        runtimes().lock().unwrap().insert(
            task_id,
            RuntimeHandle {
                client: RpcClient {
                    tx: commands,
                    actor: Arc::new(Mutex::new(Some(actor))),
                },
                owner: SessionInstance::from_session(&old_session),
                scope: old_session.cwd.clone(),
                started_at: Instant::now(),
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
            turn_id: None,
            usage: None,
            created_at: now,
            updated_at: now,
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
                        StartAcceptance::Accepted(_)
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
            turn_id: Some("turn-1".to_owned()),
            usage: None,
            created_at: now,
            updated_at: now,
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
                        ControlAcceptance::Accepted(_)
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
            turn_id: Some("turn-1".to_owned()),
            usage: None,
            created_at: now,
            updated_at: now,
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
                    Ok(ControlAcceptance::Accepted(_)) => true,
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
            turn_id: Some("turn-1".to_owned()),
            usage: None,
            created_at: config::unix_time(),
            updated_at: config::unix_time(),
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
        runtimes().lock().unwrap().insert(
            runtime_task_id,
            RuntimeHandle {
                client: RpcClient {
                    tx: commands,
                    actor: Arc::new(Mutex::new(Some(actor))),
                },
                owner: SessionInstance::from_session(&owner),
                scope: owner.cwd.clone(),
                started_at: Instant::now(),
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
            assert!(matches!(accepted, ControlAcceptance::Accepted(_)));
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

    #[tokio::test]
    async fn fake_app_server_handshake_start_read_steer_interrupt_and_evidence() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "fake", true);
        let binary = fake_app_server(root.path(), "ok");
        let task_id = Uuid::new_v4();
        let (client, initialized) =
            spawn_initialized_client_with_binary_mode(&session, Some(task_id), &binary, false)
                .await
                .unwrap();
        assert_eq!(initialized["platformOs"], "macos");
        let models = client
            .request("model/list", json!({"includeHidden":true}))
            .await
            .unwrap();
        validate_model_request(&models, "gpt-5.6-luna", "max").unwrap();
        let thread = client
            .request(
                "thread/start",
                json!({"cwd":session.cwd,"sandbox":"workspaceWrite"}),
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
    async fn app_server_rejects_incompatible_and_oversized_protocol() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), "protocol", true);
        let incompatible = root.path().join("fake-app-server-incompatible");
        std::fs::write(
            &incompatible,
            "#!/usr/bin/env python3\nimport json,sys\nfor line in sys.stdin:\n r=json.loads(line)\n if r.get('method')=='initialize': print(json.dumps({'id':r['id'],'result':{'userAgent':'temote-mcp/9.9.9','codexHome':'/tmp','platformFamily':'unix','platformOs':'macos'}}),flush=True)\n",
        )
        .unwrap();
        std::fs::set_permissions(&incompatible, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = spawn_initialized_client_with_binary_mode(&session, None, &incompatible, false)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("CODEX_APP_SERVER_INCOMPATIBLE"));

        let oversized = root.path().join("fake-app-server-oversized");
        std::fs::write(
            &oversized,
            format!("#!/usr/bin/env python3\nimport sys,json\nfor line in sys.stdin:\n sys.stdout.write('x'*{}+'\\n');sys.stdout.flush()\n", MAX_RPC_LINE_BYTES + 1),
        )
        .unwrap();
        std::fs::set_permissions(&oversized, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = spawn_initialized_client_with_binary_mode(&session, None, &oversized, false)
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
}
