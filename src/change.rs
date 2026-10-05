//! Durable, session-scoped Change identity and single-writer correlation.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MAX_RECORD: u64 = 128 * 1024;
const MAX_EXECUTIONS: usize = 128;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeScope {
    pub session_id: String,
    pub session_started_at: u64,
    pub session_process_id: u32,
    pub canonical_root: PathBuf,
    pub repository_id: String,
}

impl ChangeScope {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.canonical_root.is_absolute()
                && self.canonical_root.canonicalize()? == self.canonical_root,
            "Change scope must be canonical"
        );
        label(&self.session_id)?;
        ensure!(
            !self.repository_id.is_empty()
                && self.repository_id.len() <= 255
                && self
                    .repository_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.')),
            "invalid repository identity"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "change_id", rename_all = "snake_case")]
pub(crate) enum ChangeBase {
    OriginMain,
    Change(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionAttempt {
    pub execution_id: String,
    pub backend: String,
    pub executor: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriterLease {
    pub execution_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Verification {
    pub revision: String,
    pub record_revision: u64,
    pub task_record_revision: u64,
    pub passed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct InitialStart {
    pub operation_id: Uuid,
    pub request_fingerprint: String,
    pub snapshot_operation_id: Uuid,
    pub execution_id: String,
    #[serde(default)]
    pub snapshot_dispatch_attempted: bool,
    pub snapshot_verified: bool,
    pub vcs_operation_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeRecord {
    pub schema_version: u32,
    pub change_id: String,
    pub scope: ChangeScope,
    pub task_id: String,
    pub parent_task_id: Option<String>,
    pub parent_change_id: Option<String>,
    pub base: ChangeBase,
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub provisioning_operation_id: Option<Uuid>,
    pub allocation_operation_id: Option<Uuid>,
    pub allocation_pending: bool,
    pub executions: Vec<ExecutionAttempt>,
    #[serde(default)]
    pub initial_start: Option<InitialStart>,
    pub writer: Option<WriterLease>,
    #[serde(default)]
    pub writer_generation: u64,
    pub logical_change_id: Option<String>,
    pub materialized_revision: Option<String>,
    #[serde(default)]
    pub latest_snapshot_operation_id: Option<Uuid>,
    #[serde(default)]
    pub task_record_revision: u64,
    pub verification: Option<Verification>,
    pub delivery: Option<crate::delivery::DeliveryReceipt>,
    pub revision: u64,
}

pub(crate) fn label(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
        "invalid Change correlation identifier"
    );
    Ok(())
}

struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(crate) struct ChangeStore {
    root: PathBuf,
    scope: ChangeScope,
}

impl ChangeStore {
    pub(crate) fn open(root: &Path, scope: ChangeScope) -> Result<Self> {
        scope.validate()?;
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        ensure!(
            root.is_dir() && !fs::symlink_metadata(&root)?.file_type().is_symlink(),
            "Change store must be a directory"
        );
        let digest = Sha256::digest(serde_json::to_vec(&scope)?);
        let root = root.join(format!("scope-{digest:x}"));
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        ensure!(
            root.canonicalize()? == root,
            "Change scope path is not canonical"
        );
        Ok(Self { root, scope })
    }

    fn lock(&self) -> Result<Lock> {
        let mut options = OpenOptions::new();
        options.write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let file = options.open(self.root.join(".lock"))?;
        #[cfg(unix)]
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "Change store lock failed"
        );
        Ok(Lock(file))
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        label(id)?;
        Ok(self.root.join(format!("chg-{id}.json")))
    }

    fn read_locked(&self, id: &str) -> Result<ChangeRecord> {
        let path = self.path(id)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(path)?;
        ensure!(
            file.metadata()?.len() <= MAX_RECORD,
            "Change record exceeds size limit"
        );
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let record: ChangeRecord = serde_json::from_slice(&bytes)?;
        ensure!(
            record.schema_version == 1 && record.change_id == id && record.scope == self.scope,
            "Change ownership or schema mismatch"
        );
        Ok(record)
    }

    fn write_locked(&self, record: &ChangeRecord) -> Result<()> {
        let path = self.path(&record.change_id)?;
        let bytes = serde_json::to_vec(record)?;
        ensure!(
            bytes.len() as u64 <= MAX_RECORD,
            "Change record exceeds size limit"
        );
        let temp = self.root.join(format!(".{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn create(
        &self,
        task_id: &str,
        parent_task_id: Option<&str>,
        parent_change_id: Option<&str>,
        base: ChangeBase,
    ) -> Result<ChangeRecord> {
        self.create_idempotent(
            Uuid::new_v4(),
            task_id,
            parent_task_id,
            parent_change_id,
            base,
        )
    }

    pub(crate) fn create_idempotent(
        &self,
        operation_id: Uuid,
        task_id: &str,
        parent_task_id: Option<&str>,
        parent_change_id: Option<&str>,
        base: ChangeBase,
    ) -> Result<ChangeRecord> {
        let digest =
            Sha256::digest([b"temote-change-v1".as_slice(), operation_id.as_bytes()].concat());
        let change_id = format!("c{}", Uuid::from_slice(&digest[..16])?.simple());
        self.create_named(change_id, task_id, parent_task_id, parent_change_id, base)
    }

    /// The managed provisioning receipt already allocated this Temote Change
    /// identity. Preserve it instead of creating a second logical Change.
    pub(crate) fn create_provisioned(
        &self,
        change_id: Uuid,
        task_id: &str,
        parent_task_id: Option<&str>,
    ) -> Result<ChangeRecord> {
        self.create_named(
            change_id.to_string(),
            task_id,
            parent_task_id,
            None,
            ChangeBase::OriginMain,
        )
    }

    fn create_named(
        &self,
        change_id: String,
        task_id: &str,
        parent_task_id: Option<&str>,
        parent_change_id: Option<&str>,
        base: ChangeBase,
    ) -> Result<ChangeRecord> {
        label(&change_id)?;
        label(task_id)?;
        if let Some(id) = parent_task_id {
            label(id)?;
        }
        if let Some(id) = parent_change_id {
            label(id)?;
        }
        ensure!(
            parent_change_id
                .is_none_or(|parent| { matches!(&base, ChangeBase::Change(id) if id == parent) }),
            "base_change and parent_change_id conflict"
        );
        let _guard = self.lock()?;
        if let ChangeBase::Change(ref id) = base {
            self.read_locked(id)?;
        }
        if let Some(id) = parent_change_id {
            self.read_locked(id)?;
        }
        if self.path(&change_id)?.exists() {
            let existing = self.read_locked(&change_id)?;
            ensure!(
                existing.task_id == task_id
                    && existing.parent_task_id.as_deref() == parent_task_id
                    && existing.parent_change_id.as_deref() == parent_change_id
                    && existing.base == base,
                "Change creation operation conflict"
            );
            return Ok(existing);
        }
        ensure!(
            !self
                .list_locked()?
                .iter()
                .any(|record| record.task_id == task_id),
            "task already owns a Change in this owner scope"
        );
        let record = ChangeRecord {
            schema_version: 1,
            change_id,
            scope: self.scope.clone(),
            task_id: task_id.into(),
            parent_task_id: parent_task_id.map(str::to_owned),
            parent_change_id: parent_change_id.map(str::to_owned),
            base,
            workspace_id: None,
            provisioning_operation_id: None,
            allocation_operation_id: None,
            allocation_pending: false,
            executions: Vec::new(),
            initial_start: None,
            writer: None,
            writer_generation: 0,
            logical_change_id: None,
            materialized_revision: None,
            latest_snapshot_operation_id: None,
            task_record_revision: 0,
            verification: None,
            delivery: None,
            revision: 1,
        };
        self.write_locked(&record)?;
        Ok(record)
    }

    pub(crate) fn get(&self, id: &str) -> Result<ChangeRecord> {
        let _guard = self.lock()?;
        self.read_locked(id)
    }

    pub(crate) fn list(&self) -> Result<Vec<ChangeRecord>> {
        let _guard = self.lock()?;
        let mut result = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|n| n.strip_prefix("chg-"))
                .and_then(|n| n.strip_suffix(".json"))
            else {
                continue;
            };
            result.push(self.read_locked(id)?);
            ensure!(result.len() <= 4096, "Change projection exceeds size limit");
        }
        result.sort_by(|a, b| a.change_id.cmp(&b.change_id));
        Ok(result)
    }

    pub(crate) fn update<F>(&self, id: &str, expected_revision: u64, f: F) -> Result<ChangeRecord>
    where
        F: FnOnce(&mut ChangeRecord) -> Result<()>,
    {
        let _guard = self.lock()?;
        let mut record = self.read_locked(id)?;
        ensure!(
            record.revision == expected_revision,
            "Change record revision conflict"
        );
        let before = serde_json::to_vec(&record)?;
        f(&mut record)?;
        ensure!(
            record.change_id == id && record.scope == self.scope && record.schema_version == 1,
            "Change immutable identity changed"
        );
        if serde_json::to_vec(&record)? != before {
            record.revision = record
                .revision
                .checked_add(1)
                .context("Change revision overflow")?;
            self.write_locked(&record)?;
        }
        Ok(record)
    }

    #[cfg(test)]
    pub(crate) fn bind_workspace(
        &self,
        id: &str,
        rev: u64,
        workspace: &str,
    ) -> Result<ChangeRecord> {
        label(workspace)?;
        let _guard = self.lock()?;
        for other in self.list_locked()? {
            if other.change_id != id && other.workspace_id.as_deref() == Some(workspace) {
                anyhow::bail!("workspace already owned by another Change");
            }
        }
        let mut record = self.read_locked(id)?;
        ensure!(record.revision == rev, "Change record revision conflict");
        if let Some(existing) = &record.workspace_id {
            ensure!(existing == workspace, "Change workspace conflict");
            return Ok(record);
        }
        record.workspace_id = Some(workspace.into());
        record.allocation_pending = false;
        record.revision += 1;
        self.write_locked(&record)?;
        Ok(record)
    }

    fn list_locked(&self) -> Result<Vec<ChangeRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let name = entry?.file_name();
            if let Some(id) = name
                .to_str()
                .and_then(|n| n.strip_prefix("chg-"))
                .and_then(|n| n.strip_suffix(".json"))
            {
                records.push(self.read_locked(id)?);
            }
        }
        Ok(records)
    }

    pub(crate) fn allocation_intent(
        &self,
        id: &str,
        rev: u64,
        operation_id: Uuid,
    ) -> Result<ChangeRecord> {
        self.update(id, rev, |r| {
            ensure!(r.workspace_id.is_none(), "Change already allocated");
            ensure!(
                r.allocation_operation_id.is_none()
                    || r.allocation_operation_id == Some(operation_id),
                "Change allocation operation conflict"
            );
            r.allocation_operation_id = Some(operation_id);
            r.allocation_pending = true;
            Ok(())
        })
    }

    /// Complete a workspace allocation only from the exact accepted managed
    /// provisioning receipt. The receipt names a separate execution session;
    /// it does not add that workspace to the Change owner's permissions.
    pub(crate) fn bind_managed_workspace(
        &self,
        id: &str,
        rev: u64,
        operation_id: Uuid,
        workspace: &str,
    ) -> Result<ChangeRecord> {
        label(workspace)?;
        let _guard = self.lock()?;
        ensure!(
            !self.list_locked()?.iter().any(|other| {
                other.change_id != id && other.workspace_id.as_deref() == Some(workspace)
            }),
            "workspace already owned by another Change"
        );
        let mut record = self.read_locked(id)?;
        ensure!(record.revision == rev, "Change record revision conflict");
        ensure!(
            record.allocation_operation_id == Some(operation_id),
            "Change allocation operation conflict"
        );
        if let Some(existing) = &record.workspace_id {
            ensure!(existing == workspace, "Change workspace conflict");
            ensure!(
                record.provisioning_operation_id == Some(operation_id),
                "Change provisioning receipt conflict"
            );
            return Ok(record);
        }
        ensure!(record.allocation_pending, "Change allocation has no intent");
        record.workspace_id = Some(workspace.to_owned());
        record.provisioning_operation_id = Some(operation_id);
        record.allocation_pending = false;
        record.revision = record
            .revision
            .checked_add(1)
            .context("Change revision overflow")?;
        self.write_locked(&record)?;
        Ok(record)
    }

    pub(crate) fn append_execution(
        &self,
        id: &str,
        rev: u64,
        attempt: ExecutionAttempt,
    ) -> Result<ChangeRecord> {
        label(&attempt.execution_id)?;
        label(&attempt.backend)?;
        label(&attempt.executor)?;
        self.update(id, rev, |r| {
            ensure!(
                r.workspace_id.is_some() && !r.allocation_pending,
                "Change workspace not allocated"
            );
            if let Some(old) = r
                .executions
                .iter()
                .find(|e| e.execution_id == attempt.execution_id)
            {
                ensure!(old == &attempt, "execution identity conflict");
                return Ok(());
            }
            if let Some(previous) = r.executions.last() {
                ensure!(
                    attempt.generation > previous.generation,
                    "execution generation did not advance"
                );
            }
            ensure!(
                r.executions.len() < MAX_EXECUTIONS,
                "execution history full"
            );
            r.executions.push(attempt);
            Ok(())
        })
    }

    /// The first mutating task is reserved before its backend can start. A
    /// retry must carry the same operation and complete request fingerprint.
    pub(crate) fn reserve_initial_start(
        &self,
        id: &str,
        rev: u64,
        start: InitialStart,
    ) -> Result<ChangeRecord> {
        self.update(id, rev, |r| {
            ensure!(
                r.workspace_id.is_some() && !r.allocation_pending,
                "Change workspace not allocated"
            );
            if let Some(existing) = &r.initial_start {
                ensure!(
                    existing.operation_id == start.operation_id
                        && existing.request_fingerprint == start.request_fingerprint
                        && existing.snapshot_operation_id == start.snapshot_operation_id
                        && existing.execution_id == start.execution_id,
                    "initial Change start operation conflict"
                );
                return Ok(());
            }
            ensure!(
                r.executions.is_empty() && r.writer.is_none(),
                "Change already has an execution"
            );
            r.initial_start = Some(start);
            Ok(())
        })
    }

    pub(crate) fn verify_initial_snapshot(
        &self,
        id: &str,
        rev: u64,
        operation_id: Uuid,
        logical: &str,
        materialized: &str,
        vcs_operation_id: &str,
    ) -> Result<ChangeRecord> {
        label(logical)?;
        label(materialized)?;
        label(vcs_operation_id)?;
        self.update(id, rev, |r| {
            let start = r
                .initial_start
                .as_mut()
                .context("initial Change start missing")?;
            ensure!(
                start.snapshot_operation_id == operation_id,
                "initial snapshot operation conflict"
            );
            ensure!(
                start.snapshot_dispatch_attempted,
                "initial snapshot was never dispatched"
            );
            if start.snapshot_verified {
                ensure!(
                    r.logical_change_id.as_deref() == Some(logical)
                        && r.materialized_revision.as_deref() == Some(materialized)
                        && start.vcs_operation_id.as_deref() == Some(vcs_operation_id),
                    "initial snapshot result conflict"
                );
                return Ok(());
            }
            ensure!(
                r.executions.is_empty() && r.writer.is_none(),
                "initial snapshot followed task startup"
            );
            start.snapshot_verified = true;
            start.vcs_operation_id = Some(vcs_operation_id.to_owned());
            r.logical_change_id = Some(logical.to_owned());
            r.materialized_revision = Some(materialized.to_owned());
            r.latest_snapshot_operation_id = Some(operation_id);
            Ok(())
        })
    }

    pub(crate) fn mark_initial_snapshot_attempt(
        &self,
        id: &str,
        rev: u64,
        operation_id: Uuid,
    ) -> Result<ChangeRecord> {
        self.update(id, rev, |r| {
            let start = r
                .initial_start
                .as_mut()
                .context("initial Change start missing")?;
            ensure!(
                start.snapshot_operation_id == operation_id,
                "initial snapshot operation conflict"
            );
            ensure!(
                !start.snapshot_verified && r.executions.is_empty() && r.writer.is_none(),
                "initial snapshot dispatch followed task startup"
            );
            start.snapshot_dispatch_attempted = true;
            Ok(())
        })
    }

    pub(crate) fn handoff_writer(
        &self,
        id: &str,
        rev: u64,
        previous: Option<&WriterLease>,
        execution_id: &str,
    ) -> Result<ChangeRecord> {
        label(execution_id)?;
        self.update(id, rev, |r| {
            ensure!(
                r.workspace_id.is_some() && !r.allocation_pending,
                "Change workspace not allocated"
            );
            ensure!(
                r.executions
                    .last()
                    .is_some_and(|e| e.execution_id == execution_id),
                "writer execution is not current"
            );
            ensure!(
                previous.is_none() && r.writer.is_none(),
                "writer must be released before handoff"
            );
            let generation = r
                .writer_generation
                .checked_add(1)
                .context("writer generation overflow")?;
            r.writer_generation = generation;
            r.writer = Some(WriterLease {
                execution_id: execution_id.into(),
                generation,
            });
            Ok(())
        })
    }

    pub(crate) fn release_writer(
        &self,
        id: &str,
        rev: u64,
        expected: &WriterLease,
    ) -> Result<ChangeRecord> {
        self.update(id, rev, |r| {
            ensure!(r.writer.as_ref() == Some(expected), "writer fence mismatch");
            r.writer = None;
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn observe_revision(
        &self,
        id: &str,
        rev: u64,
        logical: &str,
        materialized: &str,
    ) -> Result<ChangeRecord> {
        label(logical)?;
        label(materialized)?;
        self.update(id, rev, |r| {
            if let Some(old) = &r.logical_change_id {
                ensure!(old == logical, "logical change identity conflict");
            }
            r.logical_change_id = Some(logical.into());
            if r.materialized_revision.as_deref() != Some(materialized) {
                r.materialized_revision = Some(materialized.into());
                r.verification = None;
            }
            Ok(())
        })
    }

    pub(crate) fn observe_task_revision(
        &self,
        id: &str,
        rev: u64,
        task_revision: u64,
    ) -> Result<ChangeRecord> {
        ensure!(task_revision > 0, "task record revision must be positive");
        self.update(id, rev, |r| {
            ensure!(
                task_revision >= r.task_record_revision,
                "task record revision regressed"
            );
            if task_revision != r.task_record_revision {
                r.task_record_revision = task_revision;
                r.verification = None;
            }
            Ok(())
        })
    }

    pub(crate) fn observe_snapshot(
        &self,
        id: &str,
        rev: u64,
        operation_id: Uuid,
        logical: &str,
        materialized: &str,
    ) -> Result<ChangeRecord> {
        label(logical)?;
        label(materialized)?;
        self.update(id, rev, |r| {
            if let Some(old) = &r.logical_change_id {
                ensure!(old == logical, "logical change identity conflict");
            }
            r.logical_change_id = Some(logical.into());
            if r.materialized_revision.as_deref() != Some(materialized)
                || r.latest_snapshot_operation_id != Some(operation_id)
            {
                r.materialized_revision = Some(materialized.into());
                r.latest_snapshot_operation_id = Some(operation_id);
                r.verification = None;
            }
            Ok(())
        })
    }

    pub(crate) fn verify(
        &self,
        id: &str,
        rev: u64,
        materialized: &str,
        passed: bool,
    ) -> Result<ChangeRecord> {
        self.update(id, rev, |r| {
            ensure!(
                r.materialized_revision.as_deref() == Some(materialized),
                "verification target is stale"
            );
            ensure!(r.task_record_revision > 0, "task record revision missing");
            if r.verification
                .as_ref()
                .is_some_and(|v| v.revision == materialized && v.passed == passed)
            {
                return Ok(());
            }
            r.verification = Some(Verification {
                revision: materialized.into(),
                record_revision: r.revision + 1,
                task_record_revision: r.task_record_revision,
                passed,
            });
            Ok(())
        })
    }

    pub(crate) fn release_ready(&self, id: &str) -> Result<bool> {
        let r = self.get(id)?;
        Ok(!r.allocation_pending
            && r.writer.is_none()
            && r.delivery.as_ref().is_none_or(|d| d.is_terminal()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, ChangeStore) {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        let scope = ChangeScope {
            session_id: "session".into(),
            session_started_at: 1,
            session_process_id: 2,
            canonical_root: canonical.clone(),
            repository_id: "repo".into(),
        };
        let store = ChangeStore::open(&canonical.join("changes"), scope).unwrap();
        (temp, store)
    }

    #[test]
    fn creation_allocation_and_handoff_survive_replay() {
        let (_tmp, store) = fixture();
        let op = Uuid::new_v4();
        let first = store
            .create_idempotent(
                op,
                "task-a",
                Some("parent-task"),
                None,
                ChangeBase::OriginMain,
            )
            .unwrap();
        assert_eq!(
            first.change_id,
            store
                .create_idempotent(
                    op,
                    "task-a",
                    Some("parent-task"),
                    None,
                    ChangeBase::OriginMain
                )
                .unwrap()
                .change_id
        );
        assert!(
            store
                .create_idempotent(op, "task-b", None, None, ChangeBase::OriginMain)
                .is_err()
        );
        let allocation = Uuid::new_v4();
        let intent = store
            .allocation_intent(&first.change_id, first.revision, allocation)
            .unwrap();
        let allocated = store
            .bind_managed_workspace(&first.change_id, intent.revision, allocation, "workspace-a")
            .unwrap();
        let attempt = ExecutionAttempt {
            execution_id: "exec-a".into(),
            backend: "codex".into(),
            executor: "agent-a".into(),
            generation: 1,
        };
        let appended = store
            .append_execution(&first.change_id, allocated.revision, attempt.clone())
            .unwrap();
        assert!(
            store
                .append_execution(&first.change_id, allocated.revision, attempt)
                .is_err()
        );
        let writer = store
            .handoff_writer(&first.change_id, appended.revision, None, "exec-a")
            .unwrap();
        assert_eq!(writer.writer.as_ref().unwrap().generation, 1);
        assert!(
            store
                .handoff_writer(&first.change_id, writer.revision, None, "exec-a")
                .is_err()
        );
        let released = store
            .release_writer(
                &first.change_id,
                writer.revision,
                writer.writer.as_ref().unwrap(),
            )
            .unwrap();
        let reacquired = store
            .handoff_writer(&first.change_id, released.revision, None, "exec-a")
            .unwrap();
        assert_eq!(reacquired.writer.as_ref().unwrap().generation, 2);
        let second = store
            .create("task-b", None, None, ChangeBase::OriginMain)
            .unwrap();
        assert!(
            store
                .bind_workspace(
                    &second.change_id,
                    second.revision,
                    allocated.workspace_id.as_deref().unwrap()
                )
                .is_err()
        );
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn scope_replacement_cannot_read_prior_instance() {
        let (_tmp, store) = fixture();
        let record = store
            .create("task", None, None, ChangeBase::OriginMain)
            .unwrap();
        let mut scope = store.scope.clone();
        scope.session_started_at += 1;
        let replacement = ChangeStore::open(store.root.parent().unwrap(), scope).unwrap();
        assert!(replacement.get(&record.change_id).is_err());
        assert!(replacement.list().unwrap().is_empty());
    }

    #[test]
    fn provisioning_change_identity_replays_only_same_task() {
        let (_tmp, store) = fixture();
        let id = Uuid::new_v4();
        let first = store.create_provisioned(id, "task-a", None).unwrap();
        assert_eq!(first.change_id, id.to_string());
        assert_eq!(
            store
                .create_provisioned(id, "task-a", None)
                .unwrap()
                .change_id,
            first.change_id
        );
        assert!(store.create_provisioned(id, "task-b", None).is_err());
    }

    #[test]
    fn initial_start_replays_exactly_and_snapshot_precedes_writer() {
        let (_tmp, store) = fixture();
        let task = Uuid::new_v4().to_string();
        let created = store
            .create(&task, None, None, ChangeBase::OriginMain)
            .unwrap();
        let bound = store
            .bind_workspace(&created.change_id, created.revision, "workspace-a")
            .unwrap();
        let start = InitialStart {
            operation_id: Uuid::new_v4(),
            request_fingerprint: "request-a".into(),
            snapshot_operation_id: Uuid::new_v4(),
            execution_id: Uuid::new_v4().to_string(),
            snapshot_dispatch_attempted: false,
            snapshot_verified: false,
            vcs_operation_id: None,
        };
        let reserved = store
            .reserve_initial_start(&bound.change_id, bound.revision, start.clone())
            .unwrap();
        assert_eq!(
            store
                .reserve_initial_start(&bound.change_id, reserved.revision, start.clone())
                .unwrap()
                .revision,
            reserved.revision
        );
        let mut changed = start.clone();
        changed.request_fingerprint = "request-b".into();
        assert!(
            store
                .reserve_initial_start(&bound.change_id, reserved.revision, changed)
                .is_err()
        );
        assert!(
            store
                .verify_initial_snapshot(
                    &bound.change_id,
                    reserved.revision,
                    Uuid::new_v4(),
                    "logical",
                    "revision",
                    "operation"
                )
                .is_err()
        );
        assert!(
            !reserved
                .initial_start
                .as_ref()
                .unwrap()
                .snapshot_dispatch_attempted
        );
        let attempted = store
            .mark_initial_snapshot_attempt(
                &bound.change_id,
                reserved.revision,
                start.snapshot_operation_id,
            )
            .unwrap();
        assert!(
            store
                .get(&bound.change_id)
                .unwrap()
                .initial_start
                .unwrap()
                .snapshot_dispatch_attempted
        );
        assert_eq!(
            store
                .mark_initial_snapshot_attempt(
                    &bound.change_id,
                    attempted.revision,
                    start.snapshot_operation_id
                )
                .unwrap()
                .revision,
            attempted.revision
        );
        let verified = store
            .verify_initial_snapshot(
                &bound.change_id,
                attempted.revision,
                start.snapshot_operation_id,
                "logical",
                "revision",
                "operation",
            )
            .unwrap();
        assert!(verified.initial_start.as_ref().unwrap().snapshot_verified);
        assert_eq!(
            store
                .verify_initial_snapshot(
                    &bound.change_id,
                    verified.revision,
                    start.snapshot_operation_id,
                    "logical",
                    "revision",
                    "operation"
                )
                .unwrap()
                .revision,
            verified.revision
        );
        assert!(
            store
                .verify_initial_snapshot(
                    &bound.change_id,
                    verified.revision,
                    start.snapshot_operation_id,
                    "logical",
                    "other",
                    "operation"
                )
                .is_err()
        );
        let executed = store
            .append_execution(
                &bound.change_id,
                verified.revision,
                ExecutionAttempt {
                    execution_id: start.execution_id.clone(),
                    backend: "codex".into(),
                    executor: "codex".into(),
                    generation: 1,
                },
            )
            .unwrap();
        let writer = store
            .handoff_writer(
                &bound.change_id,
                executed.revision,
                None,
                &start.execution_id,
            )
            .unwrap();
        assert_eq!(
            writer.writer.as_ref().unwrap().execution_id,
            start.execution_id
        );
    }

    #[test]
    fn sibling_managed_receipts_keep_distinct_workspaces_and_owner_scope() {
        let (_tmp, store) = fixture();
        let first_op = Uuid::new_v4();
        let second_op = Uuid::new_v4();
        let first = store
            .create_idempotent(first_op, "task-a", None, None, ChangeBase::OriginMain)
            .unwrap();
        let second = store
            .create_idempotent(second_op, "task-b", None, None, ChangeBase::OriginMain)
            .unwrap();
        let first = store
            .allocation_intent(&first.change_id, first.revision, first_op)
            .unwrap();
        let second = store
            .allocation_intent(&second.change_id, second.revision, second_op)
            .unwrap();
        let first = store
            .bind_managed_workspace(&first.change_id, first.revision, first_op, "workspace-a")
            .unwrap();
        assert!(
            store
                .bind_managed_workspace(
                    &second.change_id,
                    second.revision,
                    second_op,
                    "workspace-a"
                )
                .is_err()
        );
        let second = store
            .bind_managed_workspace(&second.change_id, second.revision, second_op, "workspace-b")
            .unwrap();
        assert_eq!(first.scope, second.scope);
        assert_ne!(first.workspace_id, second.workspace_id);
        assert_eq!(
            store
                .bind_managed_workspace(&first.change_id, first.revision, first_op, "workspace-a")
                .unwrap()
                .revision,
            first.revision
        );
        assert!(
            store
                .bind_managed_workspace(&first.change_id, first.revision, second_op, "workspace-a")
                .is_err()
        );
        assert!(
            store
                .create_idempotent(Uuid::new_v4(), "task-a", None, None, ChangeBase::OriginMain,)
                .is_err()
        );
    }

    #[test]
    fn explicit_parent_conflict_fails_before_creation() {
        let (_tmp, store) = fixture();
        let parent = store
            .create("parent", None, None, ChangeBase::OriginMain)
            .unwrap();
        assert!(
            store
                .create(
                    "child",
                    Some("parent"),
                    Some(&parent.change_id),
                    ChangeBase::OriginMain,
                )
                .is_err()
        );
        let operation = Uuid::new_v4();
        store
            .create_idempotent(operation, "child", None, None, ChangeBase::OriginMain)
            .unwrap();
        assert!(
            store
                .create_idempotent(
                    operation,
                    "child",
                    None,
                    Some(&parent.change_id),
                    ChangeBase::Change(parent.change_id.clone()),
                )
                .is_err()
        );
    }

    #[test]
    fn concurrent_sibling_bindings_remain_isolated() {
        let (_tmp, store) = fixture();
        let first = store
            .create("task-first", None, None, ChangeBase::OriginMain)
            .unwrap();
        let second = store
            .create("task-second", None, None, ChangeBase::OriginMain)
            .unwrap();
        let first_op = Uuid::new_v4();
        let second_op = Uuid::new_v4();
        let first = store
            .allocation_intent(&first.change_id, first.revision, first_op)
            .unwrap();
        let second = store
            .allocation_intent(&second.change_id, second.revision, second_op)
            .unwrap();
        let (first, second) = std::thread::scope(|scope| {
            let left = scope.spawn(|| {
                store.bind_managed_workspace(
                    &first.change_id,
                    first.revision,
                    first_op,
                    "workspace-first",
                )
            });
            let right = scope.spawn(|| {
                store.bind_managed_workspace(
                    &second.change_id,
                    second.revision,
                    second_op,
                    "workspace-second",
                )
            });
            (
                left.join().unwrap().unwrap(),
                right.join().unwrap().unwrap(),
            )
        });
        assert_ne!(first.workspace_id, second.workspace_id);
        assert_eq!(first.scope, second.scope);
    }
}
