use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config;

const CHECKPOINT_SCHEMA_VERSION: u64 = 1;
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
const MAX_LIST_SCAN: usize = 128;
const MAX_LIST_DIRECTORY_ENTRIES: usize = 4096;
const MAX_STEPS: usize = 64;
const MAX_CHECKS: usize = 64;
const MAX_TEXT_BYTES: usize = 256;
const MAX_IDENTIFIER_BYTES: usize = 64;
const MAX_OPERATION_HISTORY: usize = 128;
const CHECKPOINT_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x5a, 0x1a, 0x7e, 0x0e, 0x34, 0xd4, 0x4c, 0x3d, 0x9f, 0x18, 0x96, 0x6d, 0xa1, 0xa5, 0x9e, 0x51,
]);
const REQUEST_FINGERPRINT_NAMESPACE: Uuid = Uuid::from_bytes([
    0x7f, 0x15, 0x50, 0x69, 0xe1, 0xa7, 0x4e, 0x77, 0xb2, 0x08, 0xe0, 0x66, 0xa3, 0x0f, 0x3a, 0x47,
]);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportedStatus {
    Pending,
    InProgress,
    Implemented,
    Verified,
    Blocked,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportedResult {
    Pass,
    Fail,
    NotRun,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointStep {
    pub id: String,
    pub description: String,
    pub reported_status: ReportedStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointCheck {
    pub step_id: String,
    pub name: String,
    pub reported_result: ReportedResult,
    pub commit: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClientCheckpoint {
    pub title: String,
    pub base_commit: Option<String>,
    pub steps: Vec<CheckpointStep>,
    pub checks: Vec<CheckpointCheck>,
    pub next_step_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct OriginSession {
    pub id: String,
    pub started_at: u64,
    pub process_id: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperationReceipt {
    pub operation_id: Uuid,
    pub request_fingerprint: Uuid,
    pub result_revision: u64,
    pub origin_session: OriginSession,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointEnvelope {
    pub schema_version: u64,
    pub checkpoint_id: Uuid,
    pub revision: u64,
    pub scope_cwd: PathBuf,
    pub origin_session: OriginSession,
    pub source: String,
    #[serde(default)]
    pub operation_id: Option<Uuid>,
    #[serde(default)]
    pub operations: Vec<OperationReceipt>,
    pub checkpoint: ClientCheckpoint,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SaveRequest {
    pub session_id: String,
    pub operation_id: Uuid,
    pub checkpoint_id: Option<Uuid>,
    pub expected_revision: u64,
    pub checkpoint: ClientCheckpoint,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoadRequest {
    pub session_id: String,
    pub checkpoint_id: Uuid,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CheckpointListEntry {
    pub checkpoint_id: Uuid,
    pub revision: u64,
    pub title: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CheckpointList {
    pub checkpoints: Vec<CheckpointListEntry>,
    pub truncated: bool,
    pub incomplete: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Store {
    directory: PathBuf,
    #[cfg(test)]
    fail_atomic_write_before_rename: bool,
}

impl Store {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            #[cfg(test)]
            fail_atomic_write_before_rename: false,
        }
    }

    #[cfg(test)]
    fn with_atomic_write_failure(mut self) -> Self {
        self.fail_atomic_write_before_rename = true;
        self
    }

    pub(crate) fn default_store() -> Result<Self> {
        Ok(Self::new(config::state_dir()?.join("work-checkpoints")))
    }

    #[cfg(test)]
    pub(crate) fn save(
        &self,
        session: &config::Session,
        checkpoint_id: Option<Uuid>,
        expected_revision: u64,
        checkpoint: ClientCheckpoint,
    ) -> Result<CheckpointEnvelope> {
        self.save_idempotent(
            session,
            Uuid::new_v4(),
            checkpoint_id,
            expected_revision,
            checkpoint,
        )
    }

    pub(crate) fn save_idempotent(
        &self,
        session: &config::Session,
        operation_id: Uuid,
        checkpoint_id: Option<Uuid>,
        expected_revision: u64,
        checkpoint: ClientCheckpoint,
    ) -> Result<CheckpointEnvelope> {
        validate_checkpoint(&checkpoint)?;
        ensure_canonical_session_scope(session)?;
        self.ensure_directory()?;

        let is_new = checkpoint_id.is_none();
        let checkpoint_id =
            checkpoint_id.unwrap_or_else(|| checkpoint_id_for_operation(session, operation_id));
        let fingerprint =
            request_fingerprint(checkpoint_id, is_new, expected_revision, &checkpoint)?;
        let _lock = self.acquire_lock(checkpoint_id)?;
        let path = self.record_path(checkpoint_id);

        let current = match std::fs::symlink_metadata(&path) {
            Ok(_) => Some(self.read_record(checkpoint_id)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("cannot inspect checkpoint record"),
        };

        if let Some(current) = current.as_ref() {
            ensure_scope_matches(current, session)?;
            if let Some((index, receipt)) = current
                .operations
                .iter()
                .enumerate()
                .find(|(_, receipt)| receipt.operation_id == operation_id)
            {
                anyhow::ensure!(
                    receipt.request_fingerprint == fingerprint,
                    "OPERATION_CONFLICT: operation_id was already committed with a different request"
                );
                let mut replay = current.clone();
                replay.revision = receipt.result_revision;
                replay.origin_session = receipt.origin_session.clone();
                replay.operation_id = Some(operation_id);
                replay.operations.truncate(index + 1);
                replay.checkpoint = checkpoint;
                return Ok(replay);
            }
            anyhow::ensure!(
                !is_new,
                "OPERATION_CONFLICT: operation_id maps to an existing checkpoint but no matching operation record is available"
            );
        }

        let revision = match current.as_ref() {
            Some(current) => {
                anyhow::ensure!(
                    current.revision == expected_revision,
                    "CHECKPOINT_CONFLICT: expected revision {expected_revision}, current revision {}",
                    current.revision
                );
                current
                    .revision
                    .checked_add(1)
                    .context("checkpoint revision overflow")?
            }
            None => {
                anyhow::ensure!(
                    expected_revision == 0,
                    "CHECKPOINT_CONFLICT: new checkpoints require expected_revision=0"
                );
                1
            }
        };

        let origin_session = OriginSession {
            id: session.id.clone(),
            started_at: session.started_at,
            process_id: session.process_id,
        };
        let mut operations = current
            .as_ref()
            .map(|current| current.operations.clone())
            .unwrap_or_default();
        operations.push(OperationReceipt {
            operation_id,
            request_fingerprint: fingerprint,
            result_revision: revision,
            origin_session: origin_session.clone(),
        });
        if operations.len() > MAX_OPERATION_HISTORY {
            let excess = operations.len() - MAX_OPERATION_HISTORY;
            operations.drain(..excess);
        }

        reject_symlink_target(&path)?;
        let envelope = CheckpointEnvelope {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            checkpoint_id,
            revision,
            scope_cwd: session.cwd.clone(),
            origin_session,
            source: "client_reported".to_owned(),
            operation_id: Some(operation_id),
            operations,
            checkpoint,
        };
        self.atomic_write(&path, &envelope)?;
        Ok(envelope)
    }

    pub(crate) fn load(
        &self,
        session: &config::Session,
        checkpoint_id: Uuid,
    ) -> Result<CheckpointEnvelope> {
        ensure_canonical_session_scope(session)?;
        self.ensure_existing_directory()?;
        let record = self.read_record(checkpoint_id)?;
        ensure_scope_matches(&record, session)?;
        Ok(record)
    }

    pub(crate) fn list_for_scope(&self, session: &config::Session) -> Result<CheckpointList> {
        ensure_canonical_session_scope(session)?;
        match self.ensure_existing_directory() {
            Ok(()) => {}
            Err(error) if is_not_found(&error) => {
                return Ok(CheckpointList {
                    checkpoints: Vec::new(),
                    truncated: false,
                    incomplete: false,
                });
            }
            Err(error) => return Err(error),
        }

        let entries = std::fs::read_dir(&self.directory).with_context(|| {
            format!("cannot list checkpoint store {}", self.directory.display())
        })?;
        let mut candidates = Vec::new();
        let mut scanned_entries = 0usize;
        for entry in entries {
            scanned_entries = scanned_entries
                .checked_add(1)
                .context("checkpoint directory entry count overflow")?;
            anyhow::ensure!(
                scanned_entries <= MAX_LIST_DIRECTORY_ENTRIES,
                "checkpoint store contains more than {MAX_LIST_DIRECTORY_ENTRIES} directory entries"
            );
            let entry = entry.context("cannot read checkpoint store directory entry")?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(stem) else {
                continue;
            };
            if id.to_string() == stem {
                candidates.push((name, id));
            }
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0));
        let truncated = candidates.len() > MAX_LIST_SCAN;
        candidates.truncate(MAX_LIST_SCAN);

        let mut checkpoints = Vec::new();
        let mut incomplete = false;
        for (_, checkpoint_id) in candidates {
            match self.read_record(checkpoint_id) {
                Ok(record) if record.scope_cwd == session.cwd => {
                    checkpoints.push(CheckpointListEntry {
                        checkpoint_id,
                        revision: record.revision,
                        title: record.checkpoint.title,
                    });
                }
                Ok(_) => {}
                Err(_) => incomplete = true,
            }
        }
        Ok(CheckpointList {
            checkpoints,
            truncated,
            incomplete,
        })
    }

    fn ensure_directory(&self) -> Result<()> {
        if let Some(parent) = self.directory.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("cannot create checkpoint store parent {}", parent.display())
            })?;
        }
        match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => validate_store_directory(&self.directory, &metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&self.directory).with_context(|| {
                    format!(
                        "cannot create checkpoint store {}",
                        self.directory.display()
                    )
                })?;
                std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))?;
                let metadata = std::fs::symlink_metadata(&self.directory)?;
                validate_store_directory(&self.directory, &metadata)
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "cannot inspect checkpoint store {}",
                    self.directory.display()
                )
            }),
        }
    }

    fn ensure_existing_directory(&self) -> Result<()> {
        let metadata = std::fs::symlink_metadata(&self.directory)
            .with_context(|| format!("checkpoint store not found: {}", self.directory.display()))?;
        validate_store_directory(&self.directory, &metadata)
    }

    fn record_path(&self, checkpoint_id: Uuid) -> PathBuf {
        self.directory.join(format!("{checkpoint_id}.json"))
    }

    fn lock_path(&self, checkpoint_id: Uuid) -> PathBuf {
        self.directory.join(format!("{checkpoint_id}.lock"))
    }

    fn acquire_lock(&self, checkpoint_id: Uuid) -> Result<FileLock> {
        let path = self.lock_path(checkpoint_id);
        reject_symlink_target(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("cannot open checkpoint lock {}", path.display()))?;
        validate_private_regular_file(&path, &file.metadata()?)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::bail!("CHECKPOINT_BUSY: checkpoint is currently being updated");
            }
            return Err(error).context("cannot lock checkpoint record");
        }
        Ok(FileLock { file })
    }

    fn read_record(&self, checkpoint_id: Uuid) -> Result<CheckpointEnvelope> {
        let path = self.record_path(checkpoint_id);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!("CHECKPOINT_NOT_FOUND: checkpoint was not found")
                } else {
                    anyhow::Error::new(error).context("checkpoint could not be opened safely")
                }
            })?;
        let metadata = file.metadata()?;
        validate_private_regular_file(&path, &metadata)?;
        anyhow::ensure!(
            metadata.len() <= MAX_CHECKPOINT_BYTES as u64,
            "checkpoint record exceeds {MAX_CHECKPOINT_BYTES} bytes"
        );
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_CHECKPOINT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= MAX_CHECKPOINT_BYTES,
            "checkpoint record exceeds {MAX_CHECKPOINT_BYTES} bytes"
        );
        let raw: serde_json::Value =
            serde_json::from_slice(&bytes).context("invalid checkpoint record")?;
        validate_checkpoint_json_shape(
            raw.get("checkpoint")
                .context("invalid checkpoint record: missing checkpoint")?,
        )?;
        let envelope: CheckpointEnvelope =
            serde_json::from_value(raw).context("invalid checkpoint record")?;
        validate_envelope(&envelope, checkpoint_id)?;
        Ok(envelope)
    }

    fn atomic_write(&self, path: &Path, envelope: &CheckpointEnvelope) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(envelope)?;
        anyhow::ensure!(
            bytes.len() <= MAX_CHECKPOINT_BYTES,
            "checkpoint record exceeds {MAX_CHECKPOINT_BYTES} bytes"
        );
        let temporary = self.directory.join(format!(
            ".{}.{}.tmp",
            envelope.checkpoint_id,
            Uuid::new_v4()
        ));
        let mut cleanup = TemporaryFile::new(temporary.clone());
        (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)
                .with_context(|| {
                    format!(
                        "cannot create checkpoint temporary file {}",
                        temporary.display()
                    )
                })?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            #[cfg(test)]
            if self.fail_atomic_write_before_rename {
                anyhow::bail!("injected atomic write failure before rename");
            }
            reject_symlink_target(path)?;
            std::fs::rename(&temporary, path)
                .with_context(|| format!("cannot replace checkpoint record {}", path.display()))?;
            cleanup.disarm();
            if let Ok(directory) = File::open(&self.directory) {
                let _ = directory.sync_all();
            }
            Ok(())
        })()
    }
}

fn checkpoint_id_for_operation(session: &config::Session, operation_id: Uuid) -> Uuid {
    let mut bytes = session.cwd.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(operation_id.as_bytes());
    Uuid::new_v5(&CHECKPOINT_ID_NAMESPACE, &bytes)
}

fn request_fingerprint(
    checkpoint_id: Uuid,
    is_new: bool,
    expected_revision: u64,
    checkpoint: &ClientCheckpoint,
) -> Result<Uuid> {
    #[derive(Serialize)]
    struct CanonicalRequest<'a> {
        checkpoint_id: Option<Uuid>,
        expected_revision: u64,
        checkpoint: &'a ClientCheckpoint,
    }
    let canonical = CanonicalRequest {
        checkpoint_id: (!is_new).then_some(checkpoint_id),
        expected_revision,
        checkpoint,
    };
    let bytes = serde_json::to_vec(&canonical)?;
    Ok(Uuid::new_v5(&REQUEST_FINGERPRINT_NAMESPACE, &bytes))
}

struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
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

pub(crate) fn parse_save_request(value: &serde_json::Value) -> Result<SaveRequest> {
    let object = value
        .as_object()
        .context("checkpoint_save arguments must be an object")?;
    anyhow::ensure!(
        object.get("operation_id").is_some(),
        "checkpoint_save requires operation_id"
    );
    if let Some(checkpoint_id) = object.get("checkpoint_id") {
        anyhow::ensure!(
            checkpoint_id.is_string(),
            "checkpoint_id must be a UUID string when provided"
        );
    }
    validate_checkpoint_json_shape(
        object
            .get("checkpoint")
            .context("checkpoint_save requires checkpoint")?,
    )?;
    let request: SaveRequest =
        serde_json::from_value(value.clone()).context("invalid checkpoint_save arguments")?;
    config::validate_session_id(&request.session_id)?;
    validate_checkpoint(&request.checkpoint)?;
    if request.checkpoint_id.is_none() {
        anyhow::ensure!(
            request.expected_revision == 0,
            "CHECKPOINT_CONFLICT: new checkpoints require expected_revision=0"
        );
    }
    Ok(request)
}

pub(crate) fn parse_load_request(value: &serde_json::Value) -> Result<LoadRequest> {
    let request: LoadRequest =
        serde_json::from_value(value.clone()).context("invalid checkpoint_load arguments")?;
    config::validate_session_id(&request.session_id)?;
    Ok(request)
}

pub(crate) fn load(session: &config::Session, checkpoint_id: Uuid) -> Result<CheckpointEnvelope> {
    Store::default_store()?.load(session, checkpoint_id)
}

pub(crate) fn approval_detail(checkpoint: &ClientCheckpoint) -> String {
    format!(
        "client-reported checkpoint; steps: {}; checks: {}",
        checkpoint.steps.len(),
        checkpoint.checks.len()
    )
}

pub(crate) fn activity_detail(checkpoint: &ClientCheckpoint) -> String {
    format!(
        "steps: {}; checks: {}",
        checkpoint.steps.len(),
        checkpoint.checks.len()
    )
}

fn validate_checkpoint_json_shape(value: &serde_json::Value) -> Result<()> {
    let object = value.as_object().context("checkpoint must be an object")?;
    for required in ["title", "base_commit", "steps", "checks", "next_step_id"] {
        anyhow::ensure!(
            object.contains_key(required),
            "checkpoint is missing required field {required}"
        );
    }
    let checks = object
        .get("checks")
        .and_then(serde_json::Value::as_array)
        .context("checkpoint checks must be an array")?;
    for check in checks {
        let check = check
            .as_object()
            .context("checkpoint check must be an object")?;
        anyhow::ensure!(
            check.contains_key("commit"),
            "checkpoint check is missing required field commit"
        );
    }
    Ok(())
}

fn validate_checkpoint(checkpoint: &ClientCheckpoint) -> Result<()> {
    anyhow::ensure!(
        checkpoint.title.len() <= MAX_TEXT_BYTES,
        "checkpoint title exceeds {MAX_TEXT_BYTES} UTF-8 bytes"
    );
    validate_hash(checkpoint.base_commit.as_deref(), "base_commit")?;
    anyhow::ensure!(
        checkpoint.steps.len() <= MAX_STEPS,
        "checkpoint contains more than {MAX_STEPS} steps"
    );
    anyhow::ensure!(
        checkpoint.checks.len() <= MAX_CHECKS,
        "checkpoint contains more than {MAX_CHECKS} checks"
    );

    let mut step_ids = HashSet::new();
    for step in &checkpoint.steps {
        validate_identifier(&step.id, "step.id")?;
        anyhow::ensure!(
            step.description.len() <= MAX_TEXT_BYTES,
            "step description exceeds {MAX_TEXT_BYTES} UTF-8 bytes"
        );
        anyhow::ensure!(
            step_ids.insert(step.id.as_str()),
            "duplicate step.id: {}",
            step.id
        );
    }
    for check in &checkpoint.checks {
        validate_identifier(&check.step_id, "check.step_id")?;
        validate_identifier(&check.name, "check.name")?;
        anyhow::ensure!(
            step_ids.contains(check.step_id.as_str()),
            "check.step_id references an unknown step"
        );
        validate_hash(check.commit.as_deref(), "check.commit")?;
    }
    if let Some(next_step_id) = checkpoint.next_step_id.as_deref() {
        validate_identifier(next_step_id, "next_step_id")?;
        anyhow::ensure!(
            step_ids.contains(next_step_id),
            "next_step_id references an unknown step"
        );
    }

    for step in checkpoint
        .steps
        .iter()
        .filter(|step| step.reported_status == ReportedStatus::Verified)
    {
        let checks = checkpoint
            .checks
            .iter()
            .filter(|check| check.step_id == step.id)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !checks.is_empty(),
            "verified step {} must have at least one check",
            step.id
        );
        let base_commit = checkpoint
            .base_commit
            .as_deref()
            .context("verified steps require a non-null base_commit")?;
        anyhow::ensure!(
            checks.iter().all(|check| {
                check.reported_result == ReportedResult::Pass
                    && check.commit.as_deref() == Some(base_commit)
            }),
            "verified step {} requires all checks to pass at base_commit",
            step.id
        );
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES,
        "{label} must contain 1..={MAX_IDENTIFIER_BYTES} ASCII characters"
    );
    anyhow::ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "{label} accepts only ASCII letters, digits, '-' and '_'"
    );
    Ok(())
}

fn validate_hash(value: Option<&str>, label: &str) -> Result<()> {
    if let Some(value) = value {
        anyhow::ensure!(
            matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{label} must be null or a 40/64-character hexadecimal commit"
        );
    }
    Ok(())
}

fn validate_envelope(envelope: &CheckpointEnvelope, expected_id: Uuid) -> Result<()> {
    anyhow::ensure!(
        envelope.schema_version == CHECKPOINT_SCHEMA_VERSION,
        "unsupported checkpoint schema version"
    );
    anyhow::ensure!(
        envelope.checkpoint_id == expected_id,
        "checkpoint record ID mismatch"
    );
    anyhow::ensure!(
        envelope.revision > 0,
        "checkpoint revision must be positive"
    );
    anyhow::ensure!(
        envelope.source == "client_reported",
        "unsupported checkpoint source"
    );
    config::validate_session_id(&envelope.origin_session.id)?;
    let canonical_scope = config::canonical_directory(&envelope.scope_cwd)?;
    anyhow::ensure!(
        canonical_scope == envelope.scope_cwd,
        "checkpoint scope is not canonical"
    );
    anyhow::ensure!(
        envelope.operations.len() <= MAX_OPERATION_HISTORY,
        "checkpoint operation history exceeds limit"
    );
    let mut operation_ids = HashSet::new();
    for receipt in &envelope.operations {
        anyhow::ensure!(
            operation_ids.insert(receipt.operation_id),
            "duplicate checkpoint operation_id"
        );
        anyhow::ensure!(
            receipt.result_revision > 0 && receipt.result_revision <= envelope.revision,
            "invalid checkpoint operation revision"
        );
        config::validate_session_id(&receipt.origin_session.id)?;
    }
    if let Some(last) = envelope.operations.last() {
        anyhow::ensure!(
            envelope.operation_id == Some(last.operation_id),
            "checkpoint operation identity mismatch"
        );
        anyhow::ensure!(
            last.result_revision == envelope.revision,
            "checkpoint latest operation revision mismatch"
        );
    } else {
        anyhow::ensure!(
            envelope.operation_id.is_none(),
            "checkpoint operation history is missing"
        );
    }
    validate_checkpoint(&envelope.checkpoint)
}

fn ensure_canonical_session_scope(session: &config::Session) -> Result<()> {
    let canonical = config::canonical_directory(&session.cwd)?;
    anyhow::ensure!(canonical == session.cwd, "session cwd is not canonical");
    Ok(())
}

fn ensure_scope_matches(envelope: &CheckpointEnvelope, session: &config::Session) -> Result<()> {
    anyhow::ensure!(
        envelope.scope_cwd == session.cwd,
        "CHECKPOINT_NOT_FOUND: checkpoint was not found"
    );
    Ok(())
}

fn validate_store_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "checkpoint store must be a real directory"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "checkpoint store must be owner-only (mode {mode:04o}): {}",
        path.display()
    );
    Ok(())
}

fn validate_private_regular_file(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "checkpoint path is not a regular file: {}",
        path.display()
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "checkpoint file must be owner-only (mode {mode:04o})"
    );
    Ok(())
}

fn reject_symlink_target(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "checkpoint path may not be a symlink"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect checkpoint path"),
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
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn session(root: &Path, id: &str) -> config::Session {
        let cwd = config::canonical_directory(root).unwrap();
        config::Session {
            id: id.to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 123,
            process_id: 456,
            yolo: true,
        }
    }

    fn checkpoint(title: &str) -> ClientCheckpoint {
        let commit = "0123456789012345678901234567890123456789".to_owned();
        ClientCheckpoint {
            title: title.to_owned(),
            base_commit: Some(commit.clone()),
            steps: vec![CheckpointStep {
                id: "install".to_owned(),
                description: "installer validation".to_owned(),
                reported_status: ReportedStatus::InProgress,
            }],
            checks: vec![CheckpointCheck {
                step_id: "install".to_owned(),
                name: "clean-install".to_owned(),
                reported_result: ReportedResult::NotRun,
                commit: None,
            }],
            next_step_id: Some("install".to_owned()),
        }
    }

    #[test]
    fn checkpoint_round_trip_is_scoped_and_revisioned() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let first_session = session(root.path(), "first");
        let second_session = session(root.path(), "second");
        let saved = store
            .save(&first_session, None, 0, checkpoint("resume validation"))
            .unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(saved.source, "client_reported");
        let loaded = store.load(&second_session, saved.checkpoint_id).unwrap();
        assert_eq!(loaded, saved);
        let updated = store
            .save(
                &second_session,
                Some(saved.checkpoint_id),
                1,
                checkpoint("updated"),
            )
            .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.origin_session.id, "second");
    }

    #[test]
    fn checkpoint_wrong_scope_is_indistinguishable_from_missing() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let saved = store
            .save(&session(root.path(), "owner"), None, 0, checkpoint("scope"))
            .unwrap();
        let wrong = store
            .load(&session(other.path(), "other"), saved.checkpoint_id)
            .unwrap_err();
        let missing = store
            .load(&session(root.path(), "owner"), Uuid::new_v4())
            .unwrap_err();
        assert!(wrong.to_string().contains("CHECKPOINT_NOT_FOUND"));
        assert!(missing.to_string().contains("CHECKPOINT_NOT_FOUND"));
    }

    #[test]
    fn checkpoint_concurrent_same_revision_has_exactly_one_success() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let session = session(root.path(), "concurrent");
        let saved = store
            .save(&session, None, 0, checkpoint("initial"))
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for index in 0..2 {
            let barrier = Arc::clone(&barrier);
            let store = store.clone();
            let session = session.clone();
            let checkpoint_id = saved.checkpoint_id;
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store.save(
                    &session,
                    Some(checkpoint_id),
                    1,
                    checkpoint(&format!("writer-{index}")),
                )
            }));
        }
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(results.iter().any(|result| {
            result.as_ref().err().is_some_and(|error| {
                error.to_string().contains("CHECKPOINT_BUSY")
                    || error.to_string().contains("CHECKPOINT_CONFLICT")
            })
        }));
    }

    #[test]
    fn checkpoint_lock_busy_and_revision_conflict_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let session = session(root.path(), "conflict");
        let saved = store
            .save(&session, None, 0, checkpoint("initial"))
            .unwrap();

        let held = store.acquire_lock(saved.checkpoint_id).unwrap();
        let busy = store
            .save(
                &session,
                Some(saved.checkpoint_id),
                saved.revision,
                checkpoint("busy"),
            )
            .unwrap_err();
        assert!(busy.to_string().contains("CHECKPOINT_BUSY"));
        drop(held);

        let conflict = store
            .save(&session, Some(saved.checkpoint_id), 0, checkpoint("stale"))
            .unwrap_err();
        assert!(conflict.to_string().contains("CHECKPOINT_CONFLICT"));
        let loaded = store.load(&session, saved.checkpoint_id).unwrap();
        assert_eq!(loaded.revision, 1);
        assert_eq!(loaded.checkpoint.title, "initial");
    }

    #[test]
    fn checkpoint_list_directory_enumeration_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let directory = store_root.path().join("checkpoints");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        for index in 0..=MAX_LIST_DIRECTORY_ENTRIES {
            std::fs::write(directory.join(format!("noise-{index}.lock")), b"").unwrap();
        }
        let store = Store::new(directory);
        let session = session(root.path(), "bounded-directory-list");
        let error = store.list_for_scope(&session).unwrap_err();
        assert!(error.to_string().contains("directory entries"));
    }

    #[test]
    fn checkpoint_list_reports_truncation_at_128_records() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let session = session(root.path(), "truncated-list");
        for index in 0..(MAX_LIST_SCAN + 1) {
            store
                .save(
                    &session,
                    None,
                    0,
                    checkpoint(&format!("checkpoint-{index}")),
                )
                .unwrap();
        }
        let listed = store.list_for_scope(&session).unwrap();
        assert_eq!(listed.checkpoints.len(), MAX_LIST_SCAN);
        assert!(listed.truncated);
        assert!(!listed.incomplete);
    }

    #[test]
    fn checkpoint_atomic_write_failure_preserves_old_json_and_cleans_temp() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let session = session(root.path(), "atomic-failure");
        let saved = store.save(&session, None, 0, checkpoint("before")).unwrap();
        let record = store.record_path(saved.checkpoint_id);
        let before = std::fs::read(&record).unwrap();

        let failing = store.clone().with_atomic_write_failure();
        let error = failing
            .save(
                &session,
                Some(saved.checkpoint_id),
                saved.revision,
                checkpoint("after"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("injected atomic write failure"));
        assert_eq!(std::fs::read(&record).unwrap(), before);
        let temporary_files = std::fs::read_dir(&store.directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temporary_files, 0);
        let reloaded = store.load(&session, saved.checkpoint_id).unwrap();
        assert_eq!(reloaded.revision, saved.revision);
        assert_eq!(reloaded.checkpoint.title, "before");
    }

    #[test]
    fn checkpoint_create_response_loss_exact_retry_returns_original_result() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let session = session(root.path(), "retry");
        let operation_id = Uuid::new_v4();
        let first = store
            .save_idempotent(&session, operation_id, None, 0, checkpoint("same"))
            .unwrap();
        let retry = store
            .save_idempotent(&session, operation_id, None, 0, checkpoint("same"))
            .unwrap();
        assert_eq!(retry.checkpoint_id, first.checkpoint_id);
        assert_eq!(retry.revision, 1);
        assert_eq!(retry.operation_id, Some(operation_id));
        let listed = store.list_for_scope(&session).unwrap();
        assert_eq!(listed.checkpoints.len(), 1);
    }

    #[test]
    fn checkpoint_operation_id_conflict_rejects_different_payload() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let session = session(root.path(), "retry-conflict");
        let operation_id = Uuid::new_v4();
        store
            .save_idempotent(&session, operation_id, None, 0, checkpoint("first"))
            .unwrap();
        let error = store
            .save_idempotent(&session, operation_id, None, 0, checkpoint("different"))
            .unwrap_err();
        assert!(error.to_string().contains("OPERATION_CONFLICT"));
    }

    #[test]
    fn checkpoint_update_exact_retry_does_not_create_second_revision() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let session = session(root.path(), "retry-update");
        let created = store
            .save_idempotent(&session, Uuid::new_v4(), None, 0, checkpoint("first"))
            .unwrap();
        let operation_id = Uuid::new_v4();
        let updated = store
            .save_idempotent(
                &session,
                operation_id,
                Some(created.checkpoint_id),
                created.revision,
                checkpoint("second"),
            )
            .unwrap();
        let retry = store
            .save_idempotent(
                &session,
                operation_id,
                Some(created.checkpoint_id),
                created.revision,
                checkpoint("second"),
            )
            .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(retry.revision, 2);
        assert_eq!(
            store
                .load(&session, created.checkpoint_id)
                .unwrap()
                .revision,
            2
        );
    }

    #[test]
    fn checkpoint_verified_requires_passing_checks_at_base_commit() {
        let mut value = checkpoint("verified");
        value.steps[0].reported_status = ReportedStatus::Verified;
        assert!(validate_checkpoint(&value).is_err());
        value.checks[0].reported_result = ReportedResult::Pass;
        value.checks[0].commit = value.base_commit.clone();
        validate_checkpoint(&value).unwrap();
        assert_eq!(value.steps[0].reported_status, ReportedStatus::Verified);
    }

    #[test]
    fn checkpoint_validation_rejects_unknown_references_and_bad_ids() {
        let mut value = checkpoint("bad");
        value.next_step_id = Some("missing".to_owned());
        assert!(validate_checkpoint(&value).is_err());
        value = checkpoint("bad");
        value.steps[0].id = "contains space".to_owned();
        assert!(validate_checkpoint(&value).is_err());
        value = checkpoint("bad");
        value.base_commit = Some("not-a-hash".to_owned());
        assert!(validate_checkpoint(&value).is_err());
    }

    #[test]
    fn checkpoint_request_requires_nullable_fields_to_be_present() {
        let value = serde_json::json!({
            "session_id": "test",
            "operation_id": Uuid::new_v4(),
            "expected_revision": 0,
            "checkpoint": {
                "title": "missing-base",
                "steps": [],
                "checks": [],
                "next_step_id": null
            }
        });
        assert!(parse_save_request(&value).is_err());

        let value = serde_json::json!({
            "session_id": "test",
            "operation_id": Uuid::new_v4(),
            "expected_revision": 0,
            "checkpoint": {
                "title": "missing-commit",
                "base_commit": null,
                "steps": [{"id":"step","description":"d","reported_status":"pending"}],
                "checks": [{"step_id":"step","name":"check","reported_result":"not_run"}],
                "next_step_id": "step"
            }
        });
        assert!(parse_save_request(&value).is_err());

        let value = serde_json::json!({
            "session_id": "test",
            "operation_id": Uuid::new_v4(),
            "checkpoint_id": null,
            "expected_revision": 0,
            "checkpoint": {
                "title": "null-id",
                "base_commit": null,
                "steps": [],
                "checks": [],
                "next_step_id": null
            }
        });
        assert!(parse_save_request(&value).is_err());
    }

    #[test]
    fn checkpoint_request_rejects_unknown_fields() {
        let value = serde_json::json!({
            "session_id": "test",
            "operation_id": Uuid::new_v4(),
            "expected_revision": 0,
            "checkpoint": checkpoint("ok"),
            "extra": true
        });
        assert!(parse_save_request(&value).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_store_rejects_symlink_directory_and_record() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let target = store_root.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let link = store_root.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(Store::new(link).ensure_directory().is_err());

        let directory = store_root.path().join("records");
        let store = Store::new(directory.clone());
        let session = session(root.path(), "symlink");
        let saved = store.save(&session, None, 0, checkpoint("target")).unwrap();
        let record = store.record_path(saved.checkpoint_id);
        std::fs::remove_file(&record).unwrap();
        let outside = store_root.path().join("outside.json");
        std::fs::write(&outside, b"{}").unwrap();
        symlink(&outside, &record).unwrap();
        assert!(store.load(&session, saved.checkpoint_id).is_err());
    }

    #[test]
    fn checkpoint_reader_rejects_oversize_invalid_json_and_schema() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let session = session(root.path(), "reader");
        let saved = store.save(&session, None, 0, checkpoint("valid")).unwrap();
        let path = store.record_path(saved.checkpoint_id);

        std::fs::write(&path, vec![b'x'; MAX_CHECKPOINT_BYTES + 1]).unwrap();
        assert!(store.load(&session, saved.checkpoint_id).is_err());
        std::fs::write(&path, b"not json").unwrap();
        assert!(store.load(&session, saved.checkpoint_id).is_err());
        let mut bad = saved.clone();
        bad.schema_version = 99;
        std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(store.load(&session, saved.checkpoint_id).is_err());
    }

    #[test]
    fn checkpoint_list_is_bounded_sorted_and_marks_corruption_incomplete() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = Store::new(store_root.path().join("checkpoints"));
        let session = session(root.path(), "list");
        let mut ids = Vec::new();
        for index in 0..3 {
            ids.push(
                store
                    .save(&session, None, 0, checkpoint(&format!("title-{index}")))
                    .unwrap()
                    .checkpoint_id,
            );
        }
        let corrupt = Uuid::new_v4();
        std::fs::write(store.record_path(corrupt), b"bad").unwrap();
        let listed = store.list_for_scope(&session).unwrap();
        let mut expected = ids.clone();
        expected.sort_by_key(Uuid::to_string);
        assert_eq!(
            listed
                .checkpoints
                .iter()
                .map(|entry| entry.checkpoint_id)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(listed.incomplete);
        assert!(!listed.truncated);
    }

    #[test]
    fn checkpoint_approval_summary_never_contains_reported_text() {
        let marker = "credential-sentinel-never-log";
        let mut value = checkpoint(marker);
        value.steps[0].description = marker.to_owned();
        let approval = approval_detail(&value);
        let activity = activity_detail(&value);
        assert!(!approval.contains(marker));
        assert!(!activity.contains(marker));
        assert!(approval.contains("steps: 1"));
        assert!(approval.contains("checks: 1"));
    }
}
