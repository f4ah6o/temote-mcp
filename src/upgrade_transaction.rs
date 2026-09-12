#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::config;

pub const UPGRADE_TRANSACTION_SCHEMA_VERSION: u64 = 1;
pub const MAX_UPGRADE_TRANSACTION_BYTES: usize = 64 * 1024;
pub const MAX_UPGRADE_TRANSACTIONS: usize = 64;
const MAX_UPGRADE_TRANSACTION_STRING_BYTES: usize = 256;
const MAX_FAILURE_SUMMARY_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeTransactionState {
    Prepared,
    Committed,
    SupervisorHandoff,
    SessionsVerifying,
    IngressRestarting,
    EndpointVerifying,
    PluginReconciling,
    Completed,
    Failed,
    RolledBack,
}

impl UpgradeTransactionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::SupervisorHandoff => "supervisor_handoff",
            Self::SessionsVerifying => "sessions_verifying",
            Self::IngressRestarting => "ingress_restarting",
            Self::EndpointVerifying => "endpoint_verifying",
            Self::PluginReconciling => "plugin_reconciling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::RolledBack)
    }

    /// Position of a state along the coordinator's monotonic progress sequence.
    ///
    /// Terminal failure states have no position because they may be entered from
    /// any non-terminal state rather than in sequence.
    fn forward_order(self) -> Option<u8> {
        Some(match self {
            Self::Prepared => 0,
            Self::Committed => 1,
            Self::SupervisorHandoff => 2,
            Self::SessionsVerifying => 3,
            Self::IngressRestarting => 4,
            Self::EndpointVerifying => 5,
            Self::PluginReconciling => 6,
            Self::Completed => 7,
            Self::Failed | Self::RolledBack => return None,
        })
    }

    /// Reports whether a coordinator may move the transaction into `next`.
    ///
    /// Non-terminal states advance strictly forward along
    /// `prepared -> committed -> supervisor_handoff -> sessions_verifying ->
    /// ingress_restarting -> endpoint_verifying -> plugin_reconciling -> completed`,
    /// skipping stages that are not required. `failed`/`rolled_back` may be
    /// entered from any non-terminal state. Terminal states never transition.
    pub fn can_transition_to(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        if matches!(next, Self::Failed | Self::RolledBack) {
            return true;
        }
        match (self.forward_order(), next.forward_order()) {
            (Some(current), Some(next)) => next > current,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradeTransaction {
    pub schema: u64,
    pub transaction_id: String,
    pub source_version: String,
    pub target_version: String,
    pub host_id: String,
    pub source_boot_generation: String,
    pub state: UpgradeTransactionState,
    pub created_at: u64,
    pub updated_at: u64,
    pub reconnect_expected: bool,
    pub supervisor_handoff_required: bool,
    pub ingress_restart_required: bool,
    pub failure_summary: Option<String>,
}

impl UpgradeTransaction {
    pub fn new(
        source_version: impl Into<String>,
        target_version: impl Into<String>,
        host_id: impl Into<String>,
        source_boot_generation: impl Into<String>,
        reconnect_expected: bool,
        supervisor_handoff_required: bool,
        ingress_restart_required: bool,
    ) -> Self {
        let now = config::unix_time();
        Self {
            schema: UPGRADE_TRANSACTION_SCHEMA_VERSION,
            transaction_id: uuid::Uuid::new_v4().to_string(),
            source_version: source_version.into(),
            target_version: target_version.into(),
            host_id: host_id.into(),
            source_boot_generation: source_boot_generation.into(),
            state: UpgradeTransactionState::Prepared,
            created_at: now,
            updated_at: now,
            reconnect_expected,
            supervisor_handoff_required,
            ingress_restart_required,
            failure_summary: None,
        }
    }

    pub fn set_state(&mut self, state: UpgradeTransactionState) -> Result<()> {
        anyhow::ensure!(
            !self.state.is_terminal(),
            "upgrade transaction {} is already terminal ({})",
            self.transaction_id,
            self.state.as_str()
        );
        anyhow::ensure!(
            self.state.can_transition_to(state),
            "invalid upgrade transaction transition {} -> {}",
            self.state.as_str(),
            state.as_str()
        );
        self.state = state;
        self.updated_at = config::unix_time();
        Ok(())
    }

    /// Records a deterministic terminal failure.
    ///
    /// Any non-terminal transaction may fail; the bounded summary is validated
    /// with the same rules as a persisted transaction.
    pub fn mark_failed(&mut self, summary: impl Into<String>) -> Result<()> {
        anyhow::ensure!(
            !self.state.is_terminal(),
            "upgrade transaction {} is already terminal ({})",
            self.transaction_id,
            self.state.as_str()
        );
        let summary = summary.into();
        anyhow::ensure!(
            summary.len() <= MAX_FAILURE_SUMMARY_BYTES,
            "upgrade transaction failure summary exceeds {MAX_FAILURE_SUMMARY_BYTES} bytes"
        );
        anyhow::ensure!(
            !summary.contains('\0'),
            "upgrade transaction failure summary must not contain NUL"
        );
        self.state = UpgradeTransactionState::Failed;
        self.failure_summary = Some(summary);
        self.updated_at = config::unix_time();
        Ok(())
    }

    /// Records a terminal rollback, optionally with a bounded failure summary.
    pub fn mark_rolled_back(&mut self, summary: Option<String>) -> Result<()> {
        anyhow::ensure!(
            !self.state.is_terminal(),
            "upgrade transaction {} is already terminal ({})",
            self.transaction_id,
            self.state.as_str()
        );
        if let Some(summary) = summary {
            anyhow::ensure!(
                summary.len() <= MAX_FAILURE_SUMMARY_BYTES,
                "upgrade transaction failure summary exceeds {MAX_FAILURE_SUMMARY_BYTES} bytes"
            );
            anyhow::ensure!(
                !summary.contains('\0'),
                "upgrade transaction failure summary must not contain NUL"
            );
            self.failure_summary = Some(summary);
        }
        self.state = UpgradeTransactionState::RolledBack;
        self.updated_at = config::unix_time();
        Ok(())
    }

    /// Ordered coordinator phases this transaction must traverse after commit.
    ///
    /// Derived from the transaction's required work so a coordinator never
    /// revisits a completed stage: supervisor handoff and ingress restart are
    /// skipped when not required, while session/endpoint verification and plugin
    /// reconciliation are always included before `completed`.
    pub fn coordinator_phases(&self) -> Vec<UpgradeTransactionState> {
        let mut phases = Vec::new();
        if self.supervisor_handoff_required {
            phases.push(UpgradeTransactionState::SupervisorHandoff);
        }
        phases.push(UpgradeTransactionState::SessionsVerifying);
        if self.ingress_restart_required {
            phases.push(UpgradeTransactionState::IngressRestarting);
        }
        phases.push(UpgradeTransactionState::EndpointVerifying);
        phases.push(UpgradeTransactionState::PluginReconciling);
        phases.push(UpgradeTransactionState::Completed);
        phases
    }
}

fn upgrade_transaction_directory() -> Result<PathBuf> {
    Ok(config::state_dir()?.join("upgrade-transactions"))
}

fn ensure_directory() -> Result<PathBuf> {
    let directory = upgrade_transaction_directory()?;
    std::fs::create_dir_all(&directory).with_context(|| {
        format!(
            "failed to create upgrade transaction directory {}",
            directory.display()
        )
    })?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    Ok(directory)
}

fn validate_transaction_id(transaction_id: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(transaction_id)
        .with_context(|| format!("invalid upgrade transaction ID {transaction_id:?}"))?;
    anyhow::ensure!(
        parsed.to_string() == transaction_id,
        "upgrade transaction ID must be a canonical UUID"
    );
    Ok(())
}

fn transaction_path(transaction_id: &str) -> Result<PathBuf> {
    validate_transaction_id(transaction_id)?;
    Ok(ensure_directory()?.join(format!("{transaction_id}.json")))
}

fn lock_path(transaction_id: &str) -> Result<PathBuf> {
    validate_transaction_id(transaction_id)?;
    Ok(ensure_directory()?.join(format!("{transaction_id}.lock")))
}

fn validate_transaction(transaction: &UpgradeTransaction) -> Result<()> {
    anyhow::ensure!(
        transaction.schema == UPGRADE_TRANSACTION_SCHEMA_VERSION,
        "unsupported upgrade transaction schema"
    );
    validate_transaction_id(&transaction.transaction_id)?;
    for (name, value) in [
        ("source_version", &transaction.source_version),
        ("target_version", &transaction.target_version),
        (
            "source_boot_generation",
            &transaction.source_boot_generation,
        ),
    ] {
        anyhow::ensure!(
            !value.trim().is_empty() && value.len() <= MAX_UPGRADE_TRANSACTION_STRING_BYTES,
            "upgrade transaction {name} is empty or oversized"
        );
        anyhow::ensure!(
            !value.contains('\0'),
            "upgrade transaction {name} must not contain NUL"
        );
    }
    crate::host_identity::validate(&transaction.host_id)
        .context("invalid upgrade transaction host_id")?;
    if let Some(summary) = &transaction.failure_summary {
        anyhow::ensure!(
            summary.len() <= MAX_FAILURE_SUMMARY_BYTES,
            "upgrade transaction failure summary exceeds {MAX_FAILURE_SUMMARY_BYTES} bytes"
        );
        anyhow::ensure!(
            !summary.contains('\0'),
            "upgrade transaction failure summary must not contain NUL"
        );
    }
    Ok(())
}

pub fn write_transaction(transaction: &UpgradeTransaction) -> Result<PathBuf> {
    validate_transaction(transaction)?;
    let path = transaction_path(&transaction.transaction_id)?;
    let bytes = serde_json::to_vec_pretty(transaction)?;
    anyhow::ensure!(
        bytes.len() <= MAX_UPGRADE_TRANSACTION_BYTES,
        "upgrade transaction exceeds {MAX_UPGRADE_TRANSACTION_BYTES} bytes"
    );
    let temporary = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "failed to create upgrade transaction {}",
                    temporary.display()
                )
            })?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("failed to replace upgrade transaction {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map(|()| path)
}

pub fn read_transaction(transaction_id: &str) -> Result<UpgradeTransaction> {
    let path = transaction_path(transaction_id)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("cannot open upgrade transaction {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "upgrade transaction must be a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_UPGRADE_TRANSACTION_BYTES as u64,
        "upgrade transaction exceeds {MAX_UPGRADE_TRANSACTION_BYTES} bytes"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "upgrade transaction must be owner-only (mode {mode:04o})"
    );
    let file = file;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_UPGRADE_TRANSACTION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_UPGRADE_TRANSACTION_BYTES,
        "upgrade transaction exceeds {MAX_UPGRADE_TRANSACTION_BYTES} bytes"
    );
    let transaction: UpgradeTransaction =
        serde_json::from_slice(&bytes).context("invalid upgrade transaction")?;
    validate_transaction(&transaction)?;
    anyhow::ensure!(
        transaction.transaction_id == transaction_id,
        "upgrade transaction ID does not match its file name"
    );
    Ok(transaction)
}

pub fn remove_transaction(transaction_id: &str) -> Result<()> {
    let path = transaction_path(transaction_id)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_file(),
                "refusing to remove upgrade transaction through a non-regular file: {}",
                path.display()
            );
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove upgrade transaction {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect upgrade transaction {}", path.display())),
    }
}

pub fn list_transaction_ids() -> Result<Vec<String>> {
    let directory = ensure_directory()?;
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&directory).with_context(|| {
        format!(
            "cannot scan upgrade transactions in {}",
            directory.display()
        )
    })? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if validate_transaction_id(id).is_err() {
            continue;
        }
        ids.push(id.to_owned());
        anyhow::ensure!(
            ids.len() <= MAX_UPGRADE_TRANSACTIONS,
            "too many upgrade transactions under {}",
            directory.display()
        );
    }
    ids.sort();
    Ok(ids)
}

pub fn acquire_transaction_lock(transaction_id: &str) -> Result<UpgradeTransactionLock> {
    let path = lock_path(transaction_id)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("cannot open upgrade transaction lock {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "upgrade transaction lock must be a regular file"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "upgrade transaction lock must be owner-only (mode {mode:04o})"
    );
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    anyhow::ensure!(
        locked == 0,
        "upgrade transaction {} is owned by another live process",
        transaction_id
    );
    Ok(UpgradeTransactionLock { _file: file, path })
}

pub struct UpgradeTransactionLock {
    _file: File,
    path: PathBuf,
}

impl UpgradeTransactionLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Decision observed on the response-flush commit barrier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeCommitDecision {
    /// The transport has not yet written/flushed the `accepted` response.
    Pending,
    /// The response was written and flushed; destructive work may begin.
    Committed,
    /// The response could not be delivered; destructive work must not begin.
    Aborted,
}

/// One-shot gate that replaces timing-based ordering between an upgrade
/// response and the destructive phase that follows it.
///
/// The request handler subscribes before returning a response and keeps the
/// waiter; the transport calls [`commit`](Self::commit) only after the
/// `accepted` response has been written and flushed, or
/// [`abort`](Self::abort) when serialization/write/flush fails. The coordinator
/// then awaits the decision. A sender dropped before any decision resolves the
/// waiter to [`UpgradeCommitDecision::Aborted`], so a lost transport can never
/// authorize destructive work.
#[derive(Clone)]
pub struct UpgradeCommitBarrier {
    sender: watch::Sender<UpgradeCommitDecision>,
}

impl UpgradeCommitBarrier {
    pub fn new() -> Self {
        let (sender, _receiver) = watch::channel(UpgradeCommitDecision::Pending);
        Self { sender }
    }

    /// Records that the response was successfully written and flushed.
    pub fn commit(&self) -> Result<()> {
        self.decide(UpgradeCommitDecision::Committed)
    }

    /// Records that the response could not be delivered.
    pub fn abort(&self) -> Result<()> {
        self.decide(UpgradeCommitDecision::Aborted)
    }

    fn decide(&self, decision: UpgradeCommitDecision) -> Result<()> {
        let decided = self.sender.send_if_modified(|current| {
            if *current != UpgradeCommitDecision::Pending {
                return false;
            }
            *current = decision;
            true
        });
        anyhow::ensure!(
            decided,
            "upgrade commit barrier was already decided as {}",
            match self.decision() {
                UpgradeCommitDecision::Pending => "pending",
                UpgradeCommitDecision::Committed => "committed",
                UpgradeCommitDecision::Aborted => "aborted",
            }
        );
        Ok(())
    }

    /// Returns the current decision without awaiting.
    pub fn decision(&self) -> UpgradeCommitDecision {
        *self.sender.borrow()
    }

    /// Creates a waiter that resolves once the transport decides.
    pub fn waiter(&self) -> UpgradeCommitWaiter {
        UpgradeCommitWaiter {
            receiver: self.sender.subscribe(),
        }
    }
}

impl Default for UpgradeCommitBarrier {
    fn default() -> Self {
        Self::new()
    }
}

/// Coordinator-side view of an [`UpgradeCommitBarrier`].
pub struct UpgradeCommitWaiter {
    receiver: watch::Receiver<UpgradeCommitDecision>,
}

impl UpgradeCommitWaiter {
    /// Awaits the transport's commit/abort decision without any wall-clock delay.
    pub async fn wait(mut self) -> UpgradeCommitDecision {
        loop {
            let decision = *self.receiver.borrow_and_update();
            if decision != UpgradeCommitDecision::Pending {
                return decision;
            }
            if self.receiver.changed().await.is_err() {
                return UpgradeCommitDecision::Aborted;
            }
        }
    }
}

/// Bounded, non-secret, reconnect-safe view of a durable upgrade transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradeTransactionStatus {
    pub transaction_id: String,
    pub state: UpgradeTransactionState,
    pub terminal: bool,
    pub source_version: String,
    pub target_version: String,
    pub host_id: String,
    pub reconnect_expected: bool,
    pub supervisor_handoff_required: bool,
    pub ingress_restart_required: bool,
    pub created_at: u64,
    pub updated_at: u64,
    pub failure_summary: Option<String>,
}

impl UpgradeTransaction {
    /// Projects the transaction into the fields a reconnect-safe status response
    /// may expose, without introducing any credential or path material.
    pub fn status(&self) -> UpgradeTransactionStatus {
        UpgradeTransactionStatus {
            transaction_id: self.transaction_id.clone(),
            state: self.state,
            terminal: self.state.is_terminal(),
            source_version: self.source_version.clone(),
            target_version: self.target_version.clone(),
            host_id: self.host_id.clone(),
            reconnect_expected: self.reconnect_expected,
            supervisor_handoff_required: self.supervisor_handoff_required,
            ingress_restart_required: self.ingress_restart_required,
            created_at: self.created_at,
            updated_at: self.updated_at,
            failure_summary: self.failure_summary.clone(),
        }
    }
}

/// Deterministic disposition for a newly requested remote upgrade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpgradeApplyDisposition {
    /// No durable transaction owns the runtime; a coordinator may be started.
    StartNew,
    /// An active transaction already targets the same version; retry is idempotent.
    ExistingActive(String),
    /// An active transaction owns the runtime for a different version; fail closed.
    ConflictActive(String),
    /// The requested target was already completed; treat as a no-op.
    AlreadyCompleted(String),
}

/// Selects the most recently updated transaction deterministically.
///
/// Recency is ordered by `updated_at`, then by transaction ID so two
/// transactions touched within the same second still resolve to one record.
pub fn recent_transaction(transactions: &[UpgradeTransaction]) -> Option<&UpgradeTransaction> {
    transactions
        .iter()
        .max_by_key(|transaction| (transaction.updated_at, transaction.transaction_id.clone()))
}

/// Selects the most recently updated completed transaction, if any.
pub fn latest_completed_transaction(
    transactions: &[UpgradeTransaction],
) -> Option<&UpgradeTransaction> {
    transactions
        .iter()
        .filter(|transaction| transaction.state == UpgradeTransactionState::Completed)
        .max_by_key(|transaction| (transaction.updated_at, transaction.transaction_id.clone()))
}

/// Returns the transactions that could still own the runtime.
pub fn active_transactions(transactions: &[UpgradeTransaction]) -> Vec<&UpgradeTransaction> {
    transactions
        .iter()
        .filter(|transaction| !transaction.state.is_terminal())
        .collect()
}

/// Decides how a new remote apply request relates to durable transaction state.
///
/// A retried request for the same active target is idempotent, a different active
/// target fails closed, and an already-completed target is a no-op. This keeps
/// only one destructive transaction in charge of the runtime.
pub fn classify_apply(
    active: Option<&UpgradeTransaction>,
    latest_completed: Option<&UpgradeTransaction>,
    target_version: &str,
) -> UpgradeApplyDisposition {
    if let Some(active) = active {
        return if active.target_version == target_version {
            UpgradeApplyDisposition::ExistingActive(active.transaction_id.clone())
        } else {
            UpgradeApplyDisposition::ConflictActive(active.transaction_id.clone())
        };
    }
    if let Some(completed) =
        latest_completed.filter(|completed| completed.target_version == target_version)
    {
        return UpgradeApplyDisposition::AlreadyCompleted(completed.transaction_id.clone());
    }
    UpgradeApplyDisposition::StartNew
}

/// Read-only best-effort lookup of the most recent transaction identifier.
///
/// This never creates state directories and never fails health reporting: on any
/// unexpected condition it reports `None` rather than claiming upgrade state.
pub fn latest_transaction_id() -> Option<String> {
    let directory = upgrade_transaction_directory().ok()?;
    let entries = std::fs::read_dir(directory).ok()?;
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if validate_transaction_id(id).is_err() {
            continue;
        }
        ids.push(id.to_owned());
        if ids.len() > MAX_UPGRADE_TRANSACTIONS {
            return None;
        }
    }
    let mut transactions = Vec::new();
    for id in ids {
        if let Ok(transaction) = read_transaction(&id) {
            transactions.push(transaction);
        }
    }
    recent_transaction(&transactions).map(|transaction| transaction.transaction_id.clone())
}

/// Read-only probe for whether another process currently owns the transaction lock.
///
/// A missing lock file means no owner and is not created by this probe, so the
/// call is safe to make from diagnostics and startup classification.
pub fn transaction_lock_is_held(transaction_id: &str) -> Result<bool> {
    validate_transaction_id(transaction_id)?;
    let path = upgrade_transaction_directory()?.join(format!("{transaction_id}.lock"));
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("cannot open upgrade transaction lock {}", path.display())
            });
        }
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "upgrade transaction lock must be a regular file"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "upgrade transaction lock must be owner-only (mode {mode:04o})"
    );
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return Ok(false);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(true)
    } else {
        Err(error).context("cannot probe upgrade transaction lock")
    }
}

/// A non-terminal transaction with no live owner lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncompleteUpgradeTransaction {
    pub transaction_id: String,
    pub state: UpgradeTransactionState,
    pub target_version: String,
}

/// Classifies non-terminal transactions that no live process currently owns.
///
/// These are the candidates a later `upgrade_status`/repair invocation must
/// report as stale or incomplete instead of silently claiming success.
pub fn incomplete_upgrade_transactions() -> Result<Vec<IncompleteUpgradeTransaction>> {
    let mut incomplete = Vec::new();
    for id in list_transaction_ids()? {
        let transaction = read_transaction(&id)?;
        if transaction.state.is_terminal() {
            continue;
        }
        if transaction_lock_is_held(&id)? {
            continue;
        }
        incomplete.push(IncompleteUpgradeTransaction {
            transaction_id: transaction.transaction_id,
            state: transaction.state,
            target_version: transaction.target_version,
        });
    }
    Ok(incomplete)
}

/// Result of a single coordinator-driven phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeCoordinatorStep {
    /// The phase finished and the coordinator may advance to the next one.
    Continue,
    /// The phase cannot be completed safely; stop and record a terminal rollback.
    Rollback,
}

/// Future returned by [`UpgradeCoordinatorExecutor::execute_phase`].
pub type UpgradeCoordinatorStepFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<UpgradeCoordinatorStep>> + 'a>>;

/// Performs the destructive work for a single coordinator phase.
///
/// All durable transaction bookkeeping stays in [`run_upgrade_coordinator`]; an
/// executor only reports whether its phase completed or must roll back. An
/// executor must not write transaction state itself.
pub trait UpgradeCoordinatorExecutor {
    fn execute_phase(&mut self, phase: UpgradeTransactionState)
    -> UpgradeCoordinatorStepFuture<'_>;
}

fn bounded_failure_summary(text: &str) -> String {
    let sanitized = text.replace('\0', " ");
    if sanitized.len() <= MAX_FAILURE_SUMMARY_BYTES {
        return sanitized;
    }
    let mut end = MAX_FAILURE_SUMMARY_BYTES;
    while end > 0 && !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_owned()
}

/// Drives one durable upgrade transaction through its coordinator phases.
///
/// The transaction must already exist in `prepared` state. The coordinator
/// waits for the transport's response-flush [`UpgradeCommitBarrier`] decision
/// before any destructive phase, so an aborted or lost transport records a
/// terminal failure and never runs a phase. Each successful phase is persisted
/// before the next begins, so a crash leaves a deterministic non-success state
/// that [`incomplete_upgrade_transactions`] can report. The exclusive
/// transaction lock makes a second live coordinator fail closed.
pub async fn run_upgrade_coordinator<E: UpgradeCoordinatorExecutor>(
    transaction_id: &str,
    waiter: UpgradeCommitWaiter,
    executor: &mut E,
) -> Result<UpgradeTransactionStatus> {
    let _lock = acquire_transaction_lock(transaction_id)?;
    let mut transaction = read_transaction(transaction_id)?;
    anyhow::ensure!(
        transaction.state == UpgradeTransactionState::Prepared,
        "upgrade transaction {} is not prepared (state {})",
        transaction.transaction_id,
        transaction.state.as_str()
    );

    match waiter.wait().await {
        UpgradeCommitDecision::Committed => {}
        UpgradeCommitDecision::Aborted | UpgradeCommitDecision::Pending => {
            transaction
                .mark_failed("upgrade response was not delivered; destructive phase aborted")?;
            write_transaction(&transaction)?;
            return Ok(transaction.status());
        }
    }

    transaction.set_state(UpgradeTransactionState::Committed)?;
    write_transaction(&transaction)?;

    for phase in transaction.coordinator_phases() {
        if phase == UpgradeTransactionState::Completed {
            transaction.set_state(UpgradeTransactionState::Completed)?;
            write_transaction(&transaction)?;
            break;
        }
        match executor.execute_phase(phase).await {
            Ok(UpgradeCoordinatorStep::Continue) => {
                transaction.set_state(phase)?;
                write_transaction(&transaction)?;
            }
            Ok(UpgradeCoordinatorStep::Rollback) => {
                transaction.mark_rolled_back(Some(format!(
                    "upgrade coordinator rolled back at {}",
                    phase.as_str()
                )))?;
                write_transaction(&transaction)?;
                return Ok(transaction.status());
            }
            Err(error) => {
                transaction.mark_failed(bounded_failure_summary(&format!("{error:#}")))?;
                write_transaction(&transaction)?;
                return Ok(transaction.status());
            }
        }
    }

    Ok(transaction.status())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        transaction: UpgradeTransaction,
    }

    impl Fixture {
        fn new() -> Self {
            let mut transaction = UpgradeTransaction::new(
                "2026.8.0",
                "2026.9.0",
                "mac-main",
                "boot-generation-a",
                true,
                true,
                true,
            );
            transaction.failure_summary = None;
            Self { transaction }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let transaction_id = &self.transaction.transaction_id;
            let _ = remove_transaction(transaction_id);
            if let Ok(directory) = upgrade_transaction_directory() {
                let _ = std::fs::remove_file(directory.join(format!("{transaction_id}.lock")));
            }
        }
    }

    #[test]
    fn transaction_round_trips_every_state() {
        let mut fixture = Fixture::new();
        for state in [
            UpgradeTransactionState::Prepared,
            UpgradeTransactionState::Committed,
            UpgradeTransactionState::SupervisorHandoff,
            UpgradeTransactionState::SessionsVerifying,
            UpgradeTransactionState::IngressRestarting,
            UpgradeTransactionState::EndpointVerifying,
            UpgradeTransactionState::PluginReconciling,
        ] {
            fixture.transaction.state = state;
            write_transaction(&fixture.transaction).unwrap();
            let read = read_transaction(&fixture.transaction.transaction_id).unwrap();
            assert_eq!(read.state, state);
            assert_eq!(read.transaction_id, fixture.transaction.transaction_id);
        }
    }

    #[test]
    fn transaction_serialization_contains_only_safe_schema_fields() {
        let fixture = Fixture::new();
        let value = serde_json::to_value(&fixture.transaction).unwrap();
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for expected in [
            "schema",
            "transaction_id",
            "source_version",
            "target_version",
            "host_id",
            "source_boot_generation",
            "state",
            "created_at",
            "updated_at",
            "reconnect_expected",
            "supervisor_handoff_required",
            "ingress_restart_required",
            "failure_summary",
        ] {
            assert!(keys.iter().any(|key| key == expected), "missing {expected}");
        }
        assert_eq!(keys.len(), 13);
        let encoded = serde_json::to_string(&fixture.transaction).unwrap();
        for forbidden in ["token", "secret", "authorization", "cookie", "password"] {
            assert!(!encoded.to_ascii_lowercase().contains(forbidden));
        }
    }

    #[test]
    fn unknown_state_and_schema_are_rejected() {
        let fixture = Fixture::new();
        write_transaction(&fixture.transaction).unwrap();
        let path = transaction_path(&fixture.transaction.transaction_id).unwrap();

        let mut value = serde_json::to_value(&fixture.transaction).unwrap();
        value["state"] = serde_json::json!("teleporting");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());

        value["state"] = serde_json::json!("prepared");
        value["schema"] = serde_json::json!(999);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());
    }

    #[test]
    fn invalid_transaction_ids_are_rejected_before_path_use() {
        let repeated = "a".repeat(36);
        for id in ["", "../escape", "not-a-uuid", repeated.as_str()] {
            assert!(validate_transaction_id(id).is_err(), "accepted {id:?}");
            assert!(transaction_path(id).is_err());
            assert!(lock_path(id).is_err());
        }
    }

    #[test]
    fn terminal_states_cannot_transition_again() {
        let mut transaction = Fixture::new().transaction.clone();
        for terminal in [
            UpgradeTransactionState::Completed,
            UpgradeTransactionState::Failed,
            UpgradeTransactionState::RolledBack,
        ] {
            let mut copy = transaction.clone();
            copy.state = terminal;
            assert!(copy.set_state(UpgradeTransactionState::Committed).is_err());
        }
        assert!(!transaction.state.is_terminal());
        transaction
            .set_state(UpgradeTransactionState::Committed)
            .unwrap();
        assert_eq!(transaction.state, UpgradeTransactionState::Committed);
        assert!(transaction.updated_at >= transaction.created_at);
    }

    #[test]
    fn write_is_atomic_and_leaves_no_temporary_files() {
        let fixture = Fixture::new();
        write_transaction(&fixture.transaction).unwrap();
        write_transaction(&fixture.transaction).unwrap();
        let directory = upgrade_transaction_directory().unwrap();
        for entry in std::fs::read_dir(directory).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy().into_owned();
            assert!(
                !name.contains(&fixture.transaction.transaction_id) || !name.ends_with(".tmp"),
                "orphan temporary transaction file: {name}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_rejects_symlinks_public_modes_and_oversized_files() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let path = transaction_path(&fixture.transaction.transaction_id).unwrap();
        let target = path.with_extension("target");
        write_transaction(&fixture.transaction).unwrap();
        std::fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());
        std::fs::remove_file(&path).unwrap();

        std::fs::copy(&target, &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, vec![b'x'; MAX_UPGRADE_TRANSACTION_BYTES + 1]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&target).unwrap();
    }

    #[test]
    fn transaction_lock_is_exclusive_and_released_on_drop() {
        let fixture = Fixture::new();
        let lock = acquire_transaction_lock(&fixture.transaction.transaction_id).unwrap();
        assert_eq!(
            lock.path(),
            lock_path(&fixture.transaction.transaction_id).unwrap()
        );
        assert!(acquire_transaction_lock(&fixture.transaction.transaction_id).is_err());
        drop(lock);
        assert!(acquire_transaction_lock(&fixture.transaction.transaction_id).is_ok());

        let id = fixture.transaction.transaction_id.clone();
        let _ = remove_transaction(&id);
        if let Ok(directory) = upgrade_transaction_directory() {
            let _ = std::fs::remove_file(directory.join(format!("{id}.lock")));
        }
        drop(fixture);
    }

    #[cfg(unix)]
    #[test]
    fn transaction_lock_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let path = lock_path(&fixture.transaction.transaction_id).unwrap();
        let target = path.with_extension("target");
        std::fs::write(&target, b"").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &path).unwrap();
        assert!(acquire_transaction_lock(&fixture.transaction.transaction_id).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&target).unwrap();
    }

    #[test]
    fn list_transaction_ids_ignores_other_files() {
        let fixture = Fixture::new();
        write_transaction(&fixture.transaction).unwrap();
        let directory = upgrade_transaction_directory().unwrap();
        std::fs::write(directory.join("not-a-transaction.txt"), b"x").unwrap();
        std::fs::write(directory.join("notes.json"), b"x").unwrap();
        let ids = list_transaction_ids().unwrap();
        assert!(ids.contains(&fixture.transaction.transaction_id));
        assert!(!ids.iter().any(|id| id == "notes"));
        let _ = std::fs::remove_file(directory.join("not-a-transaction.txt"));
        let _ = std::fs::remove_file(directory.join("notes.json"));
    }

    #[test]
    fn status_view_marks_terminal_states() {
        let mut transaction = Fixture::new().transaction.clone();
        transaction.state = UpgradeTransactionState::Prepared;
        let status = transaction.status();
        assert_eq!(status.transaction_id, transaction.transaction_id);
        assert_eq!(status.target_version, transaction.target_version);
        assert!(!status.terminal);

        transaction.state = UpgradeTransactionState::Completed;
        assert!(transaction.status().terminal);

        let encoded = serde_json::to_string(&transaction.status()).unwrap();
        for forbidden in ["token", "secret", "authorization", "cookie", "password"] {
            assert!(!encoded.to_ascii_lowercase().contains(forbidden));
        }
    }

    #[test]
    fn recent_transaction_prefers_updated_at_then_id() {
        let mut older = Fixture::new().transaction.clone();
        older.updated_at = 10;
        let mut newer = Fixture::new().transaction.clone();
        newer.updated_at = 20;
        let transactions = vec![older.clone(), newer.clone()];
        assert_eq!(
            recent_transaction(&transactions).unwrap().transaction_id,
            newer.transaction_id
        );

        let mut first = Fixture::new().transaction.clone();
        let mut second = Fixture::new().transaction.clone();
        first.updated_at = 5;
        second.updated_at = 5;
        let (greater, lesser) = if first.transaction_id > second.transaction_id {
            (first, second)
        } else {
            (second, first)
        };
        let transactions = vec![lesser.clone(), greater.clone()];
        assert_eq!(
            recent_transaction(&transactions).unwrap().transaction_id,
            greater.transaction_id
        );
    }

    #[test]
    fn latest_completed_transaction_ignores_non_terminal_records() {
        let mut completed = Fixture::new().transaction.clone();
        completed.state = UpgradeTransactionState::Completed;
        completed.updated_at = 10;
        let mut active = Fixture::new().transaction.clone();
        active.state = UpgradeTransactionState::Committed;
        active.updated_at = 50;
        let transactions = vec![completed.clone(), active];
        assert_eq!(
            latest_completed_transaction(&transactions)
                .unwrap()
                .transaction_id,
            completed.transaction_id
        );

        let only_active = vec![Fixture::new().transaction.clone()];
        assert!(latest_completed_transaction(&only_active).is_none());
    }

    #[test]
    fn active_transactions_exclude_terminal_states() {
        let mut prepared = Fixture::new().transaction.clone();
        prepared.state = UpgradeTransactionState::Prepared;
        let mut completed = Fixture::new().transaction.clone();
        completed.state = UpgradeTransactionState::Completed;
        let mut failed = Fixture::new().transaction.clone();
        failed.state = UpgradeTransactionState::Failed;
        let transactions = vec![prepared.clone(), completed, failed];
        let active = active_transactions(&transactions);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].transaction_id, prepared.transaction_id);
    }

    #[test]
    fn classify_apply_is_idempotent_and_conflict_safe() {
        let mut active = Fixture::new().transaction.clone();
        active.target_version = "2026.10.0".to_owned();
        active.state = UpgradeTransactionState::Committed;
        assert_eq!(
            classify_apply(Some(&active), None, "2026.10.0"),
            UpgradeApplyDisposition::ExistingActive(active.transaction_id.clone())
        );
        assert_eq!(
            classify_apply(Some(&active), None, "2026.11.0"),
            UpgradeApplyDisposition::ConflictActive(active.transaction_id.clone())
        );

        let mut completed = Fixture::new().transaction.clone();
        completed.target_version = "2026.10.0".to_owned();
        completed.state = UpgradeTransactionState::Completed;
        assert_eq!(
            classify_apply(None, Some(&completed), "2026.10.0"),
            UpgradeApplyDisposition::AlreadyCompleted(completed.transaction_id.clone())
        );
        assert_eq!(
            classify_apply(None, Some(&completed), "2026.11.0"),
            UpgradeApplyDisposition::StartNew
        );
        assert_eq!(
            classify_apply(None, None, "2026.10.0"),
            UpgradeApplyDisposition::StartNew
        );
    }

    #[test]
    fn transaction_lock_probe_detects_live_owner_without_creating_files() {
        let fixture = Fixture::new();
        let id = fixture.transaction.transaction_id.clone();
        assert!(!transaction_lock_is_held(&id).unwrap());
        let lock = acquire_transaction_lock(&id).unwrap();
        assert!(transaction_lock_is_held(&id).unwrap());
        drop(lock);
        assert!(!transaction_lock_is_held(&id).unwrap());
    }

    #[test]
    fn state_transitions_are_monotonic_and_reject_regressions() {
        let mut transaction = Fixture::new().transaction.clone();
        assert!(
            transaction
                .state
                .can_transition_to(UpgradeTransactionState::Committed)
        );
        assert!(
            !transaction
                .state
                .can_transition_to(UpgradeTransactionState::Prepared)
        );
        // A coordinator may skip stages that are not required.
        assert!(
            transaction
                .state
                .can_transition_to(UpgradeTransactionState::EndpointVerifying)
        );

        transaction
            .set_state(UpgradeTransactionState::Committed)
            .unwrap();
        assert!(
            transaction
                .set_state(UpgradeTransactionState::Prepared)
                .is_err()
        );
        transaction
            .set_state(UpgradeTransactionState::SessionsVerifying)
            .unwrap();
        assert!(
            transaction
                .set_state(UpgradeTransactionState::SupervisorHandoff)
                .is_err()
        );
    }

    #[test]
    fn failure_and_rollback_are_reachable_from_any_non_terminal_state() {
        for state in [
            UpgradeTransactionState::Prepared,
            UpgradeTransactionState::Committed,
            UpgradeTransactionState::IngressRestarting,
            UpgradeTransactionState::PluginReconciling,
        ] {
            let mut failed = Fixture::new().transaction.clone();
            failed.state = state;
            failed.mark_failed("handoff failed").unwrap();
            assert_eq!(failed.state, UpgradeTransactionState::Failed);
            assert_eq!(failed.failure_summary.as_deref(), Some("handoff failed"));
            assert!(
                failed
                    .set_state(UpgradeTransactionState::Completed)
                    .is_err(),
                "a failed transaction must stay terminal"
            );

            let mut rolled_back = Fixture::new().transaction.clone();
            rolled_back.state = state;
            rolled_back.mark_rolled_back(None).unwrap();
            assert_eq!(rolled_back.state, UpgradeTransactionState::RolledBack);
            assert!(rolled_back.failure_summary.is_none());
        }
    }

    #[test]
    fn transaction_failure_summary_is_bounded_and_nul_free() {
        let mut transaction = Fixture::new().transaction.clone();
        assert!(
            transaction
                .mark_failed("x".repeat(MAX_FAILURE_SUMMARY_BYTES + 1))
                .is_err()
        );
        assert_eq!(transaction.state, UpgradeTransactionState::Prepared);
        assert!(transaction.mark_failed("bad\0summary").is_err());
        assert_eq!(transaction.state, UpgradeTransactionState::Prepared);
        transaction.mark_failed("bounded").unwrap();
        assert!(transaction.mark_failed("again").is_err());
    }

    #[test]
    fn coordinator_phases_skip_optional_stages_and_progress_forward() {
        let mut transaction = Fixture::new().transaction.clone();
        transaction.state = UpgradeTransactionState::Committed;
        transaction.supervisor_handoff_required = true;
        transaction.ingress_restart_required = true;
        let full = transaction.coordinator_phases();
        assert_eq!(
            full,
            vec![
                UpgradeTransactionState::SupervisorHandoff,
                UpgradeTransactionState::SessionsVerifying,
                UpgradeTransactionState::IngressRestarting,
                UpgradeTransactionState::EndpointVerifying,
                UpgradeTransactionState::PluginReconciling,
                UpgradeTransactionState::Completed,
            ]
        );
        let mut current = UpgradeTransactionState::Committed;
        for phase in full {
            assert!(
                current.can_transition_to(phase),
                "phase {} -> {} must be a valid forward transition",
                current.as_str(),
                phase.as_str()
            );
            current = phase;
        }

        transaction.supervisor_handoff_required = false;
        transaction.ingress_restart_required = false;
        let minimal = transaction.coordinator_phases();
        assert_eq!(
            minimal,
            vec![
                UpgradeTransactionState::SessionsVerifying,
                UpgradeTransactionState::EndpointVerifying,
                UpgradeTransactionState::PluginReconciling,
                UpgradeTransactionState::Completed,
            ]
        );
        assert!(
            UpgradeTransactionState::Committed.can_transition_to(minimal[0]),
            "committed must skip directly to the first required phase"
        );
    }

    #[tokio::test]
    async fn commit_barrier_commits_only_after_a_transport_decision() {
        let barrier = UpgradeCommitBarrier::new();
        assert_eq!(barrier.decision(), UpgradeCommitDecision::Pending);
        let waiter = barrier.waiter();
        barrier.commit().unwrap();
        assert_eq!(waiter.wait().await, UpgradeCommitDecision::Committed);
        assert_eq!(barrier.decision(), UpgradeCommitDecision::Committed);
        assert!(
            barrier.commit().is_err(),
            "a barrier may only be decided once"
        );
        assert!(barrier.abort().is_err());
    }

    #[tokio::test]
    async fn commit_barrier_abort_and_drop_never_authorize_work() {
        let aborted = UpgradeCommitBarrier::new();
        let abort_waiter = aborted.waiter();
        aborted.abort().unwrap();
        assert_eq!(abort_waiter.wait().await, UpgradeCommitDecision::Aborted);

        let dropped = UpgradeCommitBarrier::new();
        let dropped_waiter = dropped.waiter();
        drop(dropped);
        assert_eq!(
            dropped_waiter.wait().await,
            UpgradeCommitDecision::Aborted,
            "a lost transport must fail closed"
        );
    }

    #[test]
    fn commit_barrier_concurrent_decisions_are_one_shot() {
        let barrier = UpgradeCommitBarrier::new();
        let start = std::sync::Arc::new(std::sync::Barrier::new(3));

        let commit_barrier = barrier.clone();
        let commit_start = start.clone();
        let commit = std::thread::spawn(move || {
            commit_start.wait();
            commit_barrier.commit()
        });

        let abort_barrier = barrier.clone();
        let abort_start = start.clone();
        let abort = std::thread::spawn(move || {
            abort_start.wait();
            abort_barrier.abort()
        });

        start.wait();
        let commit = commit.join().unwrap();
        let abort = abort.join().unwrap();
        assert_eq!(
            usize::from(commit.is_ok()) + usize::from(abort.is_ok()),
            1,
            "exactly one concurrent decision may win"
        );
        assert_ne!(barrier.decision(), UpgradeCommitDecision::Pending);
    }

    struct RecordingExecutor {
        steps: Vec<UpgradeTransactionState>,
        outcome: UpgradeCoordinatorStep,
        fail_at: Option<UpgradeTransactionState>,
    }

    impl RecordingExecutor {
        fn new(outcome: UpgradeCoordinatorStep) -> Self {
            Self {
                steps: Vec::new(),
                outcome,
                fail_at: None,
            }
        }
    }

    impl UpgradeCoordinatorExecutor for RecordingExecutor {
        fn execute_phase(
            &mut self,
            phase: UpgradeTransactionState,
        ) -> UpgradeCoordinatorStepFuture<'_> {
            Box::pin(async move {
                self.steps.push(phase);
                if self.fail_at == Some(phase) {
                    anyhow::bail!("phase {} failed in test executor", phase.as_str());
                }
                Ok(self.outcome)
            })
        }
    }

    fn prepared_fixture() -> Fixture {
        let fixture = Fixture::new();
        write_transaction(&fixture.transaction).unwrap();
        fixture
    }

    #[tokio::test]
    async fn coordinator_aborts_without_running_phases_when_response_is_lost() {
        let fixture = prepared_fixture();
        let barrier = UpgradeCommitBarrier::new();
        let waiter = barrier.waiter();
        drop(barrier);
        let mut executor = RecordingExecutor::new(UpgradeCoordinatorStep::Continue);

        let id = &fixture.transaction.transaction_id;
        let status = run_upgrade_coordinator(id, waiter, &mut executor)
            .await
            .unwrap();

        assert_eq!(status.state, UpgradeTransactionState::Failed);
        assert!(status.terminal);
        assert!(
            executor.steps.is_empty(),
            "destructive phases must not run before the transport commits"
        );
        assert_eq!(
            read_transaction(&fixture.transaction.transaction_id)
                .unwrap()
                .state,
            UpgradeTransactionState::Failed
        );
    }

    #[tokio::test]
    async fn coordinator_commits_then_completes_every_required_phase_in_order() {
        let fixture = prepared_fixture();
        let barrier = UpgradeCommitBarrier::new();
        let waiter = barrier.waiter();
        barrier.commit().unwrap();
        let mut executor = RecordingExecutor::new(UpgradeCoordinatorStep::Continue);

        let id = &fixture.transaction.transaction_id;
        let status = run_upgrade_coordinator(id, waiter, &mut executor)
            .await
            .unwrap();

        assert_eq!(status.state, UpgradeTransactionState::Completed);
        assert!(status.terminal);
        let expected = fixture
            .transaction
            .coordinator_phases()
            .into_iter()
            .filter(|phase| *phase != UpgradeTransactionState::Completed)
            .collect::<Vec<_>>();
        assert_eq!(executor.steps, expected);
        assert_eq!(
            read_transaction(&fixture.transaction.transaction_id)
                .unwrap()
                .state,
            UpgradeTransactionState::Completed
        );
    }

    #[tokio::test]
    async fn coordinator_records_rollback_when_a_phase_requests_it() {
        let fixture = prepared_fixture();
        let barrier = UpgradeCommitBarrier::new();
        let waiter = barrier.waiter();
        barrier.commit().unwrap();
        let mut executor = RecordingExecutor::new(UpgradeCoordinatorStep::Rollback);

        let id = &fixture.transaction.transaction_id;
        let status = run_upgrade_coordinator(id, waiter, &mut executor)
            .await
            .unwrap();

        assert_eq!(status.state, UpgradeTransactionState::RolledBack);
        assert!(status.terminal);
        assert_eq!(
            executor.steps.len(),
            1,
            "rollback must stop before the next phase"
        );
    }

    #[tokio::test]
    async fn coordinator_records_a_bounded_failure_when_a_phase_errors() {
        let fixture = prepared_fixture();
        let barrier = UpgradeCommitBarrier::new();
        let waiter = barrier.waiter();
        barrier.commit().unwrap();
        let mut executor = RecordingExecutor::new(UpgradeCoordinatorStep::Continue);
        executor.fail_at = Some(UpgradeTransactionState::SessionsVerifying);

        let id = &fixture.transaction.transaction_id;
        let status = run_upgrade_coordinator(id, waiter, &mut executor)
            .await
            .unwrap();

        assert_eq!(status.state, UpgradeTransactionState::Failed);
        let summary = status.failure_summary.unwrap();
        assert!(summary.contains("sessions_verifying"), "{summary}");
    }

    #[tokio::test]
    async fn coordinator_fails_closed_on_a_second_owner_or_non_prepared_state() {
        let fixture = prepared_fixture();
        let id = &fixture.transaction.transaction_id;
        let held = acquire_transaction_lock(id).unwrap();
        let barrier = UpgradeCommitBarrier::new();
        barrier.commit().unwrap();
        let mut executor = RecordingExecutor::new(UpgradeCoordinatorStep::Continue);

        let result = run_upgrade_coordinator(id, barrier.waiter(), &mut executor).await;
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("owned by another live process"),
            "{error}"
        );
        drop(held);

        let mut committed = fixture.transaction.clone();
        committed.state = UpgradeTransactionState::Committed;
        write_transaction(&committed).unwrap();
        let barrier = UpgradeCommitBarrier::new();
        barrier.commit().unwrap();
        let result = run_upgrade_coordinator(id, barrier.waiter(), &mut executor).await;
        let error = result.unwrap_err();
        assert!(error.to_string().contains("not prepared"), "{error}");
    }
}
