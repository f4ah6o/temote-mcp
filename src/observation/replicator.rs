//! Host-to-Fabric replication of the owner-only observation journal.
//!
//! Local append remains the source of truth. This module reads retained
//! journals in bounded pages and advances its durable cursor only after the
//! authenticated Fabric endpoint confirms a committed ACK.

use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{config, host_identity};

use super::{INSTRUCTION_PREVIEW_BYTES, Observation, ObservationContent, ObservationStore};

const SYNC_SCHEMA_VERSION: u32 = 1;
const SYNC_STATE_DIRECTORY: &str = "gateway-observation-sync";
const MAX_BATCH_RECORDS: usize = 32;
const MAX_BATCH_BYTES: usize = 512 * 1024;
const MAX_STATE_FILE_BYTES: usize = 32 * 1024;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);
const PREVIEW_ENV: &str = "TEMOTE_MCP_OBSERVATION_SYNC_PREVIEW";

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct AckState {
    schema_version: u32,
    host_id: String,
    session_id: String,
    acked_through_revision: u64,
    #[serde(default)]
    delivered_through_revision: u64,
    #[serde(default)]
    repository_key: Option<String>,
    #[serde(default)]
    reported_base_revision: u64,
    #[serde(default)]
    reported_head_revision: u64,
    #[serde(default)]
    reported_gap_count: u64,
    #[serde(default)]
    reported_degraded: bool,
    updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct ReplicatorStatus {
    schema_version: u32,
    host_id: String,
    #[serde(default)]
    last_session_id: Option<String>,
    #[serde(default)]
    last_attempt_at: Option<u64>,
    #[serde(default)]
    last_success_at: Option<u64>,
    #[serde(default)]
    last_error_code: Option<String>,
    #[serde(default)]
    consecutive_failures: u32,
    #[serde(default)]
    retry_after_seconds: u64,
    #[serde(default)]
    retry_at: Option<u64>,
    #[serde(default)]
    sessions_seen: u64,
    #[serde(default)]
    records_acked: u64,
    #[serde(default)]
    last_cloud_head_seq: Option<u64>,
    #[serde(default)]
    last_source_complete: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SyncRequest {
    pub schema_version: u32,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_key: Option<String>,
    pub source_base_revision: u64,
    pub source_head_revision: u64,
    pub journal_degraded: bool,
    pub gap_count: u64,
    pub records: Vec<SyncRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SyncRecord {
    pub source_revision: u64,
    pub observation: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SyncResponse {
    pub session_id: String,
    pub acked_through_revision: u64,
    #[serde(default)]
    pub committed_through_revision: u64,
    pub cloud_head_seq: u64,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyncFailureCode {
    LocalStore,
    Authentication,
    RemoteRejected,
    Timeout,
    Transport,
    InvalidResponse,
    InvalidAck,
    IdentityConflict,
}

impl SyncFailureCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LocalStore => "local_store",
            Self::Authentication => "authentication",
            Self::RemoteRejected => "remote_rejected",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::InvalidResponse => "invalid_response",
            Self::InvalidAck => "invalid_ack",
            Self::IdentityConflict => "identity_conflict",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SyncFailure {
    pub code: SyncFailureCode,
}

impl SyncFailure {
    pub(crate) const fn new(code: SyncFailureCode) -> Self {
        Self { code }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SyncOutcome {
    NoWork,
    Busy,
    Backoff {
        seconds: u64,
    },
    Acked {
        session_id: String,
        records: usize,
        through_revision: u64,
    },
}

struct PendingBatch {
    request: SyncRequest,
    previous_ack: AckState,
    session_id: String,
}

pub(crate) struct HostReplicator {
    host_id: String,
    directory: PathBuf,
    store: ObservationStore,
    include_previews: bool,
    retry_at: Option<Instant>,
    retry_delay: Duration,
    status: ReplicatorStatus,
}

impl HostReplicator {
    pub(crate) fn new(host_id: &str, gateway_url: &str) -> Result<Self> {
        let host_id = host_identity::validate(host_id)?;
        let target_id = format!("{:x}", Sha256::digest(gateway_url.as_bytes()));
        let directory_root = config::state_dir()?
            .join(SYNC_STATE_DIRECTORY)
            .join(&host_id);
        ensure_private_directory(&directory_root)?;
        let directory = directory_root.join(target_id);
        ensure_private_directory(&directory)?;
        let default_status = ReplicatorStatus {
            schema_version: SYNC_SCHEMA_VERSION,
            host_id: host_id.clone(),
            ..ReplicatorStatus::default()
        };
        let mut status = read_json_file::<ReplicatorStatus>(&directory.join("status.json"))
            .unwrap_or_else(|_| default_status.clone());
        if status.schema_version != SYNC_SCHEMA_VERSION || status.host_id != host_id {
            status = default_status;
        }
        let retry_delay = if status.retry_after_seconds == 0 {
            INITIAL_RETRY_DELAY
        } else {
            Duration::from_secs(
                status
                    .retry_after_seconds
                    .saturating_mul(2)
                    .min(MAX_RETRY_DELAY.as_secs()),
            )
        };
        let now = config::unix_time();
        let retry_at = status
            .retry_at
            .filter(|retry_at| *retry_at > now)
            .map(|retry_at| Instant::now() + Duration::from_secs(retry_at - now));
        Ok(Self {
            host_id,
            directory,
            store: ObservationStore::default_store()?,
            include_previews: preview_upload_enabled(),
            retry_at,
            retry_delay,
            status,
        })
    }

    /// Sends at most one bounded batch per call. The caller invokes this on
    /// the normal host-agent lifecycle loop; sync failures never fail the
    /// accepted task or cause a backend operation to be replayed.
    pub(crate) async fn sync_next<F, Fut>(
        &mut self,
        send: F,
    ) -> std::result::Result<SyncOutcome, SyncFailure>
    where
        F: FnOnce(SyncRequest) -> Fut,
        Fut: Future<Output = std::result::Result<SyncResponse, SyncFailure>>,
    {
        let _lock = match HostSyncLock::try_acquire(&self.directory.join("replicator.lock")) {
            Ok(Some(lock)) => lock,
            Ok(None) => return Ok(SyncOutcome::Busy),
            Err(_) => return Err(self.record_failure(SyncFailureCode::LocalStore)),
        };
        if let Ok(status) = read_json_file::<ReplicatorStatus>(&self.directory.join("status.json"))
            && status.schema_version == SYNC_SCHEMA_VERSION
            && status.host_id == self.host_id
        {
            self.status = status;
            let now = config::unix_time();
            self.retry_delay = Duration::from_secs(if self.status.retry_after_seconds == 0 {
                INITIAL_RETRY_DELAY.as_secs()
            } else {
                self.status
                    .retry_after_seconds
                    .saturating_mul(2)
                    .min(MAX_RETRY_DELAY.as_secs())
            });
            self.retry_at = self
                .status
                .retry_at
                .filter(|retry_at| *retry_at > now)
                .map(|retry_at| Instant::now() + Duration::from_secs(retry_at - now));
        }
        if let Some(retry_at) = self.retry_at
            && Instant::now() < retry_at
        {
            return Ok(SyncOutcome::Backoff {
                seconds: retry_at
                    .duration_since(Instant::now())
                    .as_secs()
                    .saturating_add(1),
            });
        }

        let pending = match self.next_batch() {
            Ok(pending) => pending,
            Err(error) => {
                let code = if error.to_string() == "observation source repository identity changed"
                {
                    SyncFailureCode::IdentityConflict
                } else {
                    SyncFailureCode::LocalStore
                };
                return Err(self.record_failure(code));
            }
        };
        let Some(batch) = pending else {
            return Ok(SyncOutcome::NoWork);
        };
        self.status.last_attempt_at = Some(config::unix_time());
        self.save_status_best_effort();

        let response = match send(batch.request.clone()).await {
            Ok(response) => response,
            Err(error) => return Err(self.record_failure(error.code)),
        };
        let last_sent_revision = batch
            .request
            .records
            .last()
            .map(|record| record.source_revision)
            .unwrap_or(batch.request.source_head_revision);
        if response.session_id != batch.session_id
            || response.acked_through_revision < batch.previous_ack.acked_through_revision
            || response.acked_through_revision > batch.request.source_head_revision
            || response.committed_through_revision > last_sent_revision
        {
            return Err(self.record_failure(SyncFailureCode::InvalidAck));
        }

        let mut ack = batch.previous_ack;
        ack.acked_through_revision = ack
            .acked_through_revision
            .max(response.acked_through_revision);
        ack.delivered_through_revision = ack
            .delivered_through_revision
            .max(response.committed_through_revision)
            .max(response.acked_through_revision);
        ack.repository_key = batch.request.repository_key.clone();
        ack.reported_base_revision = batch.request.source_base_revision;
        ack.reported_head_revision = batch.request.source_head_revision;
        ack.reported_gap_count = batch.request.gap_count;
        ack.reported_degraded = batch.request.journal_degraded;
        ack.updated_at = config::unix_time();
        if write_json_atomic(&self.ack_path(&batch.session_id), &ack).is_err() {
            // D1 may have committed. Leaving the old cursor forces a safe
            // idempotent replay of the same observations after restart.
            return Err(self.record_failure(SyncFailureCode::LocalStore));
        }

        self.retry_at = None;
        self.retry_delay = INITIAL_RETRY_DELAY;
        self.status.last_success_at = Some(config::unix_time());
        self.status.last_error_code = None;
        self.status.consecutive_failures = 0;
        self.status.retry_after_seconds = 0;
        self.status.retry_at = None;
        self.status.records_acked = self.status.records_acked.saturating_add(
            batch
                .request
                .records
                .iter()
                .filter(|record| {
                    record.source_revision
                        <= response
                            .committed_through_revision
                            .max(response.acked_through_revision)
                })
                .count() as u64,
        );
        self.status.last_cloud_head_seq = Some(response.cloud_head_seq);
        self.status.last_source_complete = Some(response.complete);
        self.save_status_best_effort();
        Ok(SyncOutcome::Acked {
            session_id: batch.session_id,
            records: batch.request.records.len(),
            through_revision: ack.delivered_through_revision,
        })
    }

    fn next_batch(&mut self) -> Result<Option<PendingBatch>> {
        let sessions = self.store.retained_sessions()?;
        if sessions.is_empty() {
            return Ok(None);
        }
        self.status.sessions_seen = sessions.len() as u64;
        let start = self
            .status
            .last_session_id
            .as_deref()
            .map(|last| {
                sessions.partition_point(|session| session.as_str() <= last) % sessions.len()
            })
            .unwrap_or(0);

        for offset in 0..sessions.len() {
            let session_id = &sessions[(start + offset) % sessions.len()];
            // Advance durably before any per-session read, consistency check,
            // or serialization can fail. One corrupt source journal must not
            // trap every later scan on this session.
            self.status.last_session_id = Some(session_id.clone());
            self.save_status()?;

            let ack_path = self.ack_path(session_id);
            let mut ack = read_json_file::<AckState>(&ack_path).unwrap_or_default();
            if ack.schema_version != SYNC_SCHEMA_VERSION
                || ack.host_id != self.host_id
                || ack.session_id != *session_id
            {
                ack = AckState {
                    schema_version: SYNC_SCHEMA_VERSION,
                    host_id: self.host_id.clone(),
                    session_id: session_id.clone(),
                    ..AckState::default()
                };
            }
            let scan_after_revision = ack
                .acked_through_revision
                .max(ack.delivered_through_revision);
            let snapshot = self.store.replication_snapshot(
                session_id,
                scan_after_revision,
                MAX_BATCH_RECORDS,
            )?;
            if scan_after_revision > snapshot.head_revision {
                anyhow::bail!("observation replication cursor is ahead of its journal");
            }
            let gap_count = ack.reported_gap_count.max(
                snapshot
                    .base_revision
                    .saturating_sub(scan_after_revision)
                    .saturating_add(snapshot.gap_count)
                    .saturating_add(snapshot.corrupt_lines)
                    .saturating_add(snapshot.write_failures),
            );
            let mut records = Vec::with_capacity(snapshot.records.len());
            let mut encoded_bytes = 0usize;
            let mut repository_key: Option<Option<String>> = None;
            for observation in &snapshot.records {
                let observation_repository_key = observation.repository_key.clone();
                if repository_key
                    .as_ref()
                    .is_some_and(|known| known != &observation_repository_key)
                {
                    break;
                }
                if repository_key.is_none() {
                    repository_key = Some(observation_repository_key);
                }
                let record = SyncRecord {
                    source_revision: observation.revision,
                    observation: cloud_observation(observation, self.include_previews)?,
                };
                let bytes = serde_json::to_vec(&record)?.len();
                if !records.is_empty() && encoded_bytes.saturating_add(bytes) > MAX_BATCH_BYTES {
                    break;
                }
                anyhow::ensure!(
                    bytes <= MAX_BATCH_BYTES,
                    "one observation exceeds the cloud sync batch byte limit"
                );
                encoded_bytes = encoded_bytes.saturating_add(bytes);
                records.push(record);
            }
            let journal_degraded = ack.reported_degraded
                || snapshot.write_failures > 0
                || snapshot.corrupt_lines > 0
                || gap_count > 0;
            let metadata_changed = ack.reported_base_revision != snapshot.base_revision
                || ack.reported_head_revision != snapshot.head_revision
                || ack.reported_gap_count != gap_count
                || ack.reported_degraded != journal_degraded;
            if records.is_empty() && !metadata_changed {
                continue;
            }
            let repository_key = if records.is_empty() {
                ack.repository_key.clone()
            } else {
                repository_key.unwrap_or(None)
            };
            if !records.is_empty()
                && ack.repository_key.is_some()
                && ack.repository_key != repository_key
            {
                // One source namespace cannot be rebound from one repository
                // to another based on a changed local remote URL. Keep the
                // cursor behind so the conflict remains visible and no new
                // record is assigned to the old repository.
                anyhow::bail!("observation source repository identity changed");
            }

            return Ok(Some(PendingBatch {
                session_id: session_id.clone(),
                request: SyncRequest {
                    schema_version: SYNC_SCHEMA_VERSION,
                    session_id: session_id.clone(),
                    repository_key,
                    source_base_revision: snapshot.base_revision,
                    source_head_revision: snapshot.head_revision,
                    journal_degraded,
                    gap_count,
                    records,
                },
                previous_ack: ack,
            }));
        }
        Ok(None)
    }

    fn ack_path(&self, session_id: &str) -> PathBuf {
        self.directory.join(format!("ack-{session_id}.json"))
    }

    fn record_failure(&mut self, code: SyncFailureCode) -> SyncFailure {
        self.status.last_error_code = Some(code.as_str().to_owned());
        self.status.consecutive_failures = self.status.consecutive_failures.saturating_add(1);
        self.status.retry_after_seconds = self.retry_delay.as_secs();
        self.status.retry_at = Some(config::unix_time().saturating_add(self.retry_delay.as_secs()));
        self.retry_at = Some(Instant::now() + self.retry_delay);
        self.retry_delay = (self.retry_delay * 2).min(MAX_RETRY_DELAY);
        self.save_status_best_effort();
        SyncFailure::new(code)
    }

    fn save_status_best_effort(&self) {
        let _ = self.save_status();
    }

    fn save_status(&self) -> Result<()> {
        write_json_atomic(&self.directory.join("status.json"), &self.status)
    }
}

fn preview_upload_enabled() -> bool {
    matches!(
        temote_mcp::environment::var(PREVIEW_ENV)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true")
    )
}

fn cloud_observation(observation: &Observation, include_previews: bool) -> Result<Value> {
    let mut value = serde_json::to_value(observation)?;
    if let Some(object) = value.as_object_mut() {
        // This is a local display label and may be a directory basename.
        // It must never become a cloud identity or leave the host.
        object.remove("repository");
        object.remove("repository_key");
        object.insert(
            "content".to_owned(),
            cloud_content(&observation.content, include_previews),
        );
    }
    Ok(value)
}

fn cloud_content(content: &ObservationContent, include_previews: bool) -> Value {
    match content {
        ObservationContent::Text {
            preview,
            total_bytes,
            sha256,
            truncated,
        } => {
            let mut result = json!({
                "kind": "text",
                "preview": "",
                "total_bytes": total_bytes,
                "sha256": sha256,
                "truncated": truncated,
            });
            if include_previews {
                result["preview"] =
                    Value::String(preview.chars().take(INSTRUCTION_PREVIEW_BYTES).collect());
            }
            result
        }
        ObservationContent::View { view } => {
            if include_previews {
                json!({"kind": "view", "view": view})
            } else {
                let bytes = serde_json::to_vec(view).unwrap_or_default();
                json!({
                    "kind": "view_digest",
                    "sha256": format!("{:x}", sha2::Sha256::digest(&bytes)),
                    "total_bytes": bytes.len(),
                    "truncated": false,
                })
            }
        }
        ObservationContent::ViewDigest {
            sha256,
            total_bytes,
            truncated,
        } => json!({
            "kind": "view_digest",
            "sha256": sha256,
            "total_bytes": total_bytes,
            "truncated": truncated,
        }),
        ObservationContent::Error { preview } => {
            if include_previews {
                json!({"kind": "error", "preview": preview})
            } else {
                json!({
                    "kind": "view_digest",
                    "sha256": format!("{:x}", sha2::Sha256::digest(preview.as_bytes())),
                    "total_bytes": preview.len(),
                    "truncated": false,
                })
            }
        }
        ObservationContent::None => json!({"kind": "none"}),
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        match builder.create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot create observation sync directory {}",
                        path.display()
                    )
                });
            }
        }
    }
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "cannot inspect observation sync directory {}",
            path.display()
        )
    })?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "observation sync state is not a regular directory"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    anyhow::ensure!(
        fs::metadata(path)?.permissions().mode() & 0o077 == 0,
        "observation sync directory is not owner-only"
    );
    Ok(())
}

struct HostSyncLock {
    file: File,
}

impl HostSyncLock {
    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file(),
            "observation sync lock is not a regular file"
        );
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "observation sync lock is not owner-only"
        );
        // SAFETY: `file` stays open for the lock lifetime and the operation
        // uses only this descriptor. Nonblocking acquisition avoids stalling
        // an asynchronous host-agent task when another generation is active.
        let result = unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if result == 0 {
            return Ok(Some(Self { file }));
        }
        let error = std::io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
        {
            return Ok(None);
        }
        Err(error.into())
    }
}

impl Drop for HostSyncLock {
    fn drop(&mut self) {
        // SAFETY: this descriptor still owns the lock acquired above.
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN);
        }
    }
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("state file is missing")
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "observation sync state is not a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_STATE_FILE_BYTES as u64,
        "observation sync state is oversized"
    );
    anyhow::ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "observation sync state is not owner-only"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_STATE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_STATE_FILE_BYTES,
        "observation sync state is oversized"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("sync state path has no parent")?;
    ensure_private_directory(parent)?;
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() <= MAX_STATE_FILE_BYTES,
        "observation sync state is oversized"
    );
    let temporary = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::{
        ActorRef, ObservationKind, Provenance, SessionInstanceRef, StateRef, TargetRef,
    };
    use std::os::unix::fs::PermissionsExt;

    fn observation(revision: u64) -> Observation {
        Observation {
            id: Uuid::new_v4(),
            schema_version: 1,
            observed_at: 1,
            accepted_at: None,
            session_id: "session-test".to_owned(),
            session_instance: SessionInstanceRef {
                started_at: 1,
                process_id: 1,
            },
            repository: Some("private-checkout-name".to_owned()),
            repository_key: Some("github:owner/repo".to_owned()),
            workspace_id: None,
            task_id: None,
            execution_id: None,
            operation_id: None,
            actor: ActorRef {
                transport: "mcp-stdio".to_owned(),
                principal: None,
            },
            target: TargetRef {
                backend: "codex".to_owned(),
            },
            action: "task_start".to_owned(),
            kind: ObservationKind::Instruction,
            content: ObservationContent::Text {
                preview: "secret text must require opt-in".to_owned(),
                total_bytes: 32,
                sha256: format!("{:x}", Sha256::digest(b"secret text must require opt-in")),
                truncated: false,
            },
            state_ref: Some(StateRef::default()),
            evidence_refs: Vec::new(),
            provenance: Provenance {
                tool: "codex_task_start".to_owned(),
                source: "orchestration".to_owned(),
                control_action: None,
            },
            revision,
            dedupe_key: format!("record-{revision}"),
        }
    }

    #[test]
    fn cloud_projection_omits_local_label_and_free_text_by_default() {
        let value = cloud_observation(&observation(1), false).unwrap();
        assert!(value.get("repository").is_none());
        assert!(value.get("repository_key").is_none());
        assert_eq!(value["content"]["preview"], "");
        assert_eq!(
            value["content"]["sha256"],
            format!("{:x}", Sha256::digest(b"secret text must require opt-in"))
        );
    }

    #[test]
    fn cloud_projection_includes_bounded_preview_only_when_enabled() {
        let value = cloud_observation(&observation(1), true).unwrap();
        assert_eq!(
            value["content"]["preview"],
            "secret text must require opt-in"
        );
    }

    #[test]
    fn ack_cursor_is_atomic_owner_only_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ack.json");
        let state = AckState {
            schema_version: SYNC_SCHEMA_VERSION,
            host_id: "host-test".to_owned(),
            session_id: "session-test".to_owned(),
            acked_through_revision: 17,
            updated_at: 1,
            ..AckState::default()
        };
        write_json_atomic(&path, &state).unwrap();
        let read: AckState = read_json_file(&path).unwrap();
        assert_eq!(read.acked_through_revision, 17);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn constructor_initializes_and_reloads_versioned_retry_status() {
        let gateway_url = format!("https://{}.gateway.test", Uuid::new_v4().simple());
        let mut replicator = HostReplicator::new("host-test", &gateway_url).unwrap();
        assert_eq!(replicator.status.schema_version, SYNC_SCHEMA_VERSION);
        assert_eq!(replicator.status.host_id, "host-test");
        replicator.status.last_session_id = Some("session-cursor".to_owned());
        replicator.record_failure(SyncFailureCode::Timeout);
        let directory = replicator.directory.clone();
        drop(replicator);

        let resumed = HostReplicator::new("host-test", &gateway_url).unwrap();
        assert_eq!(resumed.status.schema_version, SYNC_SCHEMA_VERSION);
        assert_eq!(resumed.status.host_id, "host-test");
        assert_eq!(
            resumed.status.last_session_id.as_deref(),
            Some("session-cursor")
        );
        assert_eq!(resumed.status.last_error_code.as_deref(), Some("timeout"));
        assert_eq!(
            resumed.status.retry_after_seconds,
            INITIAL_RETRY_DELAY.as_secs()
        );
        assert!(resumed.retry_at.is_some());
        assert_eq!(
            resumed.retry_delay.as_secs(),
            INITIAL_RETRY_DELAY.as_secs() * 2
        );
        assert_eq!(
            read_json_file::<ReplicatorStatus>(&directory.join("status.json"))
                .unwrap()
                .schema_version,
            SYNC_SCHEMA_VERSION
        );
    }

    #[test]
    fn older_gateway_response_uses_contiguous_ack_as_delivery_fallback() {
        let response: SyncResponse = serde_json::from_value(json!({
            "session_id": "session-test",
            "acked_through_revision": 7,
            "cloud_head_seq": 11,
            "complete": true,
        }))
        .unwrap();
        assert_eq!(response.committed_through_revision, 0);
        assert_eq!(
            response
                .acked_through_revision
                .max(response.committed_through_revision),
            7
        );
    }

    #[tokio::test]
    async fn internal_gap_does_not_starve_records_after_first_bounded_page() {
        let journal_dir = tempfile::tempdir().unwrap();
        let sync_dir = tempfile::tempdir().unwrap();
        fs::set_permissions(journal_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(sync_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = ObservationStore::new(journal_dir.path().to_path_buf());
        let session_id = Uuid::new_v4().to_string();
        for revision in 1..=70 {
            let mut record = observation(revision);
            record.session_id = session_id.clone();
            record.dedupe_key = format!("test-{revision}");
            store.append(record).unwrap();
        }

        // Simulate one permanently corrupt/missing journal entry in the
        // middle while retaining more than a page of later valid records.
        let journal = journal_dir.path().join(format!("obs-{session_id}.jsonl"));
        let original = fs::read_to_string(&journal).unwrap();
        let mut retained = String::new();
        for line in original.lines() {
            let record: Observation = serde_json::from_str(line).unwrap();
            if record.revision != 2 {
                retained.push_str(line);
                retained.push('\n');
            }
        }
        fs::write(&journal, retained).unwrap();

        let host_id = "host-test".to_owned();
        ensure_private_directory(sync_dir.path()).unwrap();
        let mut replicator = HostReplicator {
            host_id: host_id.clone(),
            directory: sync_dir.path().to_path_buf(),
            store,
            include_previews: false,
            retry_at: None,
            retry_delay: INITIAL_RETRY_DELAY,
            status: ReplicatorStatus {
                schema_version: SYNC_SCHEMA_VERSION,
                host_id: host_id.clone(),
                ..ReplicatorStatus::default()
            },
        };
        let batches = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u64>>::new()));

        for _ in 0..3 {
            let batches = batches.clone();
            let outcome = replicator
                .sync_next(|request| async move {
                    let revisions = request
                        .records
                        .iter()
                        .map(|record| record.source_revision)
                        .collect::<Vec<_>>();
                    let committed_through_revision = revisions.last().copied().unwrap_or(0);
                    batches.lock().unwrap().push(revisions);
                    Ok(SyncResponse {
                        session_id: request.session_id,
                        acked_through_revision: 1,
                        committed_through_revision,
                        cloud_head_seq: committed_through_revision,
                        complete: false,
                    })
                })
                .await
                .unwrap();
            assert!(matches!(
                outcome,
                SyncOutcome::Acked { records, .. }
                    if records > 0 && records <= MAX_BATCH_RECORDS
            ));
        }

        {
            let batches = batches.lock().unwrap();
            let revisions = batches.iter().flatten().copied().collect::<Vec<_>>();
            assert_eq!(revisions.len(), 69);
            assert!(!revisions.contains(&2));
            assert_eq!(revisions.first(), Some(&1));
            assert_eq!(revisions.last(), Some(&70));
            assert!(batches.len() >= 3);
            assert!(batches.iter().all(|batch| batch.len() <= MAX_BATCH_RECORDS));
        }

        // Simulate journal compaction after all retained records were
        // delivered. The base must not turn the already reported internal
        // hole into a much larger gap merely because the contiguous D1 ACK is
        // still before that hole.
        let original = fs::read_to_string(&journal).unwrap();
        let mut compacted = String::new();
        for line in original.lines() {
            let record: Observation = serde_json::from_str(line).unwrap();
            if record.revision > 40 {
                compacted.push_str(line);
                compacted.push('\n');
            }
        }
        fs::write(&journal, compacted).unwrap();
        fs::write(
            journal_dir
                .path()
                .join(format!("obs-{session_id}.meta.json")),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "base_revision": 40,
                "write_failures": 0,
                "compactions": 1,
                "last_write_error": null,
            }))
            .unwrap(),
        )
        .unwrap();

        let outcome = replicator
            .sync_next(|request| async move {
                assert!(request.records.is_empty());
                assert_eq!(request.source_base_revision, 40);
                assert_eq!(request.gap_count, 1);
                assert!(request.journal_degraded);
                Ok(SyncResponse {
                    session_id: request.session_id,
                    acked_through_revision: 1,
                    committed_through_revision: 1,
                    cloud_head_seq: 70,
                    complete: false,
                })
            })
            .await
            .unwrap();
        assert!(matches!(outcome, SyncOutcome::Acked { records: 0, .. }));
        let ack: AckState =
            read_json_file(&sync_dir.path().join(format!("ack-{session_id}.json"))).unwrap();
        assert_eq!(ack.acked_through_revision, 1);
        assert_eq!(ack.delivered_through_revision, 70);
        assert_eq!(ack.reported_gap_count, 1);
        assert!(ack.reported_degraded);
        assert_eq!(ack.reported_base_revision, 40);

        let outcome = replicator
            .sync_next(|_| async { panic!("compaction metadata should already be reported") })
            .await
            .unwrap();
        assert_eq!(outcome, SyncOutcome::NoWork);
    }

    #[tokio::test]
    async fn repository_identity_conflict_does_not_starve_other_sessions() {
        let journal_dir = tempfile::tempdir().unwrap();
        let sync_dir = tempfile::tempdir().unwrap();
        fs::set_permissions(journal_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(sync_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = ObservationStore::new(journal_dir.path().to_path_buf());
        let host_id = "host-test".to_owned();

        let mut conflicting = observation(1);
        conflicting.session_id = "a-conflict".to_owned();
        conflicting.repository_key = Some("github:new/repository".to_owned());
        store.append(conflicting).unwrap();
        let mut healthy = observation(1);
        healthy.session_id = "b-healthy".to_owned();
        healthy.repository_key = Some("github:healthy/repository".to_owned());
        store.append(healthy).unwrap();

        write_json_atomic(
            &sync_dir.path().join("ack-a-conflict.json"),
            &AckState {
                schema_version: SYNC_SCHEMA_VERSION,
                host_id: host_id.clone(),
                session_id: "a-conflict".to_owned(),
                repository_key: Some("github:old/repository".to_owned()),
                ..AckState::default()
            },
        )
        .unwrap();

        let mut replicator = HostReplicator {
            host_id: host_id.clone(),
            directory: sync_dir.path().to_path_buf(),
            store,
            include_previews: false,
            retry_at: None,
            retry_delay: INITIAL_RETRY_DELAY,
            status: ReplicatorStatus {
                schema_version: SYNC_SCHEMA_VERSION,
                host_id,
                ..ReplicatorStatus::default()
            },
        };

        let error = replicator
            .sync_next(|_| async { panic!("identity conflict must fail before sending") })
            .await
            .unwrap_err();
        assert_eq!(error.code, SyncFailureCode::IdentityConflict);
        let persisted: ReplicatorStatus =
            read_json_file(&sync_dir.path().join("status.json")).unwrap();
        assert_eq!(persisted.last_session_id.as_deref(), Some("a-conflict"));
        assert_eq!(
            persisted.last_error_code.as_deref(),
            Some("identity_conflict")
        );

        let next = replicator.next_batch().unwrap().unwrap();
        assert_eq!(next.session_id, "b-healthy");
    }

    #[tokio::test]
    async fn journal_read_and_ahead_cursor_errors_advance_to_healthy_session() {
        let journal_dir = tempfile::tempdir().unwrap();
        let sync_dir = tempfile::tempdir().unwrap();
        fs::set_permissions(journal_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(sync_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = ObservationStore::new(journal_dir.path().to_path_buf());
        let host_id = "host-test".to_owned();
        let sessions = ["a-read-error", "b-ahead-cursor", "c-healthy"];
        for session_id in sessions {
            let mut record = observation(1);
            record.session_id = session_id.to_owned();
            record.repository_key = Some("github:owner/repository".to_owned());
            store.append(record).unwrap();
        }

        fs::set_permissions(
            journal_dir.path().join("obs-a-read-error.jsonl"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        write_json_atomic(
            &sync_dir.path().join("ack-b-ahead-cursor.json"),
            &AckState {
                schema_version: SYNC_SCHEMA_VERSION,
                host_id: host_id.clone(),
                session_id: "b-ahead-cursor".to_owned(),
                repository_key: Some("github:owner/repository".to_owned()),
                delivered_through_revision: 2,
                ..AckState::default()
            },
        )
        .unwrap();

        let mut replicator = HostReplicator {
            host_id: host_id.clone(),
            directory: sync_dir.path().to_path_buf(),
            store,
            include_previews: false,
            retry_at: None,
            retry_delay: INITIAL_RETRY_DELAY,
            status: ReplicatorStatus {
                schema_version: SYNC_SCHEMA_VERSION,
                host_id,
                ..ReplicatorStatus::default()
            },
        };

        let first_error = replicator
            .sync_next(|_| async { panic!("unreadable journal must fail before sending") })
            .await
            .unwrap_err();
        assert_eq!(first_error.code, SyncFailureCode::LocalStore);
        let persisted: ReplicatorStatus =
            read_json_file(&sync_dir.path().join("status.json")).unwrap();
        assert_eq!(persisted.last_session_id.as_deref(), Some("a-read-error"));

        let second_error = match replicator.next_batch() {
            Err(error) => error,
            Ok(_) => panic!("ahead-of-journal cursor must fail closed"),
        };
        assert!(second_error.to_string().contains("cursor is ahead"));
        let persisted: ReplicatorStatus =
            read_json_file(&sync_dir.path().join("status.json")).unwrap();
        assert_eq!(persisted.last_session_id.as_deref(), Some("b-ahead-cursor"));

        let healthy = replicator.next_batch().unwrap().unwrap();
        assert_eq!(healthy.session_id, "c-healthy");
    }
}
