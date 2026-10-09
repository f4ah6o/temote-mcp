//! Durable admission for managed repository provisioning. Repository contents
//! are created by a delegated coding-agent task, never by this store.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::session_source::{RepositoryId, VcsPreference};

const MAX_RECEIPT_BYTES: u64 = 32 * 1024;
const RECEIPT_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedRequest {
    pub repository: RepositoryId,
    pub base: Option<String>,
    pub vcs: VcsPreference,
}

impl ManagedRequest {
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(base) = &self.base {
            anyhow::ensure!(
                !base.is_empty()
                    && base.len() <= 256
                    && !base.starts_with('-')
                    && !base.contains("..")
                    && !base.contains("@{")
                    && !base.ends_with('/')
                    && !base.ends_with('.')
                    && base
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte)),
                "base must be a bounded Git ref name"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProvisioningPhase {
    Accepted,
    RepositoryPreparing,
    WorkspaceAllocating,
    EnvironmentPreparing,
    EnvironmentRetryable,
    EnvironmentUnsupported,
    WorkspaceReady,
    Failed,
    ReconciliationRequired,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActivatedSessionInstance {
    pub session_id: String,
    pub started_at: u64,
    pub process_id: u32,
    pub activity_instance_id: Uuid,
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(default)]
    pub permission_mode: crate::config::PermissionMode,
    #[serde(default)]
    pub permitted_directories: Vec<PathBuf>,
    #[serde(default)]
    pub grants: crate::config::SessionGrants,
}

impl ActivatedSessionInstance {
    pub(crate) fn from_session(
        session: &crate::config::Session,
        activity_instance_id: Uuid,
    ) -> Self {
        Self {
            session_id: session.id.clone(),
            started_at: session.started_at,
            process_id: session.process_id,
            activity_instance_id,
            cwd: session.cwd.clone(),
            permission_mode: session.permission_mode,
            permitted_directories: session.permitted_directories.clone(),
            grants: session.grants.clone(),
        }
    }

    pub(crate) fn matches(&self, session: &crate::config::Session) -> bool {
        self.session_id == session.id
            && self.started_at == session.started_at
            && self.process_id == session.process_id
            && self.cwd == session.cwd
            && self.permission_mode == session.permission_mode
            && self.permitted_directories == session.permitted_directories
            && self.grants == session.grants
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProvisioningReceipt {
    version: u8,
    pub operation_id: Uuid,
    pub accepted_at: u64,
    pub request: ManagedRequest,
    pub root_name: String,
    #[serde(default)]
    pub canonical_root: Option<PathBuf>,
    pub session_id: String,
    pub workspace_id: Uuid,
    pub change_id: Uuid,
    pub task_operation_id: Uuid,
    pub preparation_session_id: String,
    #[serde(default)]
    pub preparation_start_attempted: bool,
    #[serde(default)]
    pub preparation_owner: Option<ActivatedSessionInstance>,
    #[serde(default)]
    pub task_start_attempted: bool,
    pub runtime_instance_id: Uuid,
    #[serde(default)]
    pub runtime_start_attempted: bool,
    #[serde(default)]
    pub activated_owner: Option<ActivatedSessionInstance>,
    #[serde(default)]
    pub preparation_model: Option<String>,
    #[serde(default)]
    pub preparation_effort: Option<String>,
    #[serde(default)]
    pub environment_attempt: Option<crate::environment_preparation::PreparationReceipt>,
    #[serde(default)]
    pub environment_attempt_number: u32,
    pub phase: ProvisioningPhase,
    pub task_id: Option<Uuid>,
    pub pinned_base: Option<String>,
    pub detail: Option<String>,
}

impl ProvisioningReceipt {
    pub(crate) fn new(
        operation_id: Uuid,
        request: ManagedRequest,
        root_name: &str,
        canonical_root: PathBuf,
    ) -> Self {
        // Deterministic IDs preserve identity even if the process exits after
        // the Accepted receipt and before the next durable transition.
        let id = |label: &str| Uuid::new_v5(&operation_id, label.as_bytes());
        Self {
            version: RECEIPT_VERSION,
            operation_id,
            accepted_at: crate::config::unix_time(),
            request,
            root_name: root_name.to_owned(),
            canonical_root: Some(canonical_root),
            session_id: id("session").to_string(),
            workspace_id: id("workspace"),
            change_id: id("change"),
            task_operation_id: id("provision-task"),
            preparation_session_id: id("preparation-session").to_string(),
            preparation_start_attempted: false,
            preparation_owner: None,
            task_start_attempted: false,
            runtime_instance_id: id("runtime-instance"),
            runtime_start_attempted: false,
            activated_owner: None,
            preparation_model: None,
            preparation_effort: None,
            environment_attempt: None,
            environment_attempt_number: 0,
            phase: ProvisioningPhase::Accepted,
            task_id: None,
            pinned_base: None,
            detail: None,
        }
    }

    fn validate_identity(&self) -> Result<()> {
        crate::named_roots::validate_root_name(&self.root_name)?;
        if let Some(root) = &self.canonical_root {
            anyhow::ensure!(root.is_absolute(), "managed root identity is not absolute");
        }
        let id = |label: &str| Uuid::new_v5(&self.operation_id, label.as_bytes());
        anyhow::ensure!(
            self.session_id == id("session").to_string(),
            "managed session identity mismatch"
        );
        anyhow::ensure!(
            self.workspace_id == id("workspace"),
            "managed workspace identity mismatch"
        );
        anyhow::ensure!(
            self.change_id == id("change"),
            "managed change identity mismatch"
        );
        anyhow::ensure!(
            self.task_operation_id == id("provision-task"),
            "managed task operation identity mismatch"
        );
        anyhow::ensure!(
            self.preparation_session_id == id("preparation-session").to_string(),
            "managed preparation identity mismatch"
        );
        anyhow::ensure!(
            self.runtime_instance_id == id("runtime-instance"),
            "managed runtime identity mismatch"
        );
        if let Some(owner) = &self.activated_owner {
            anyhow::ensure!(
                owner.session_id == self.session_id
                    && owner.activity_instance_id == self.runtime_instance_id,
                "activated session owner mismatch"
            );
        }
        if let Some(owner) = &self.preparation_owner {
            anyhow::ensure!(
                owner.session_id == self.preparation_session_id
                    && owner.activity_instance_id == self.task_operation_id,
                "preparation session owner mismatch"
            );
        }
        if self.phase == ProvisioningPhase::WorkspaceReady {
            anyhow::ensure!(
                self.pinned_base.is_some()
                    && self.activated_owner.is_some()
                    && self.environment_attempt.as_ref().is_some_and(|attempt| {
                        attempt.phase == crate::environment_preparation::PreparationPhase::Ready
                            && attempt.start_attempted
                            && attempt.task_id.is_some()
                            && attempt.model.is_some()
                            && attempt.effort.is_some()
                            && self
                                .activated_owner
                                .as_ref()
                                .is_some_and(|owner| owner.cwd == attempt.plan.workspace)
                    }),
                "ready managed receipt lacks its pinned base, environment, or activated owner"
            );
        }
        if let Some(attempt) = &self.environment_attempt {
            anyhow::ensure!(
                attempt.plan.workspace_id == self.workspace_id
                    && attempt.plan.repository_id == self.request.repository
                    && attempt.plan.version == crate::environment_preparation::PREPARATION_VERSION
                    && attempt.plan.operation_id
                        == Uuid::new_v5(
                            &self.operation_id,
                            format!("environment-attempt-{}", self.environment_attempt_number)
                                .as_bytes(),
                        )
                    && self.canonical_root.as_ref().is_some_and(|root| {
                        attempt.plan.canonical_scope == *root
                            && attempt.plan.workspace
                                == root
                                    .join(crate::workspace_provisioning::workspace_relative(self))
                    })
                    && attempt.plan.session.session_id == self.preparation_session_id
                    && self.preparation_owner.as_ref().is_some_and(|owner| {
                        owner.session_id == attempt.plan.session.session_id
                            && owner.started_at == attempt.plan.session.started_at
                            && owner.process_id == attempt.plan.session.process_id
                            && owner.cwd == attempt.plan.session.canonical_scope
                            && owner.permission_mode == attempt.plan.session.permission_mode
                            && owner.permitted_directories
                                == attempt.plan.session.permitted_directories
                            && owner.grants == attempt.plan.session.grants
                    }),
                "environment attempt owner identity mismatch"
            );
        }
        Ok(())
    }
}

pub(crate) struct RepositoryStore {
    directory: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EnsurePhase {
    Owned,
    Ready,
    ReconciliationRequired,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EnsureClaim {
    repository: RepositoryId,
    operation_id: Uuid,
    phase: EnsurePhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnsureAdmission {
    Owned,
    Busy,
}

impl RepositoryStore {
    pub(crate) fn new() -> Result<Self> {
        let state = crate::config::state_dir()?;
        fs::create_dir_all(&state)?;
        Ok(Self {
            directory: fs::canonicalize(state)?.join("managed-provisioning"),
        })
    }

    #[cfg(test)]
    fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub(crate) async fn accept(
        &self,
        operation_id: Uuid,
        request: ManagedRequest,
        root_name: &str,
        canonical_root: &Path,
    ) -> Result<ProvisioningReceipt> {
        request.validate()?;
        crate::named_roots::validate_root_name(root_name)?;
        anyhow::ensure!(
            fs::canonicalize(canonical_root)? == canonical_root,
            "managed named root changed before acceptance"
        );
        let _lock = crate::config::acquire_session_lifecycle_lock().await?;
        self.ensure_directory()?;
        let path = self.path(operation_id);
        if let Some(existing) = self.read_path(&path)? {
            anyhow::ensure!(
                existing.request == request && existing.root_name == root_name,
                "operation_conflict: managed provisioning operation_id was accepted with different inputs"
            );
            return Ok(existing);
        }
        let receipt = ProvisioningReceipt::new(
            operation_id,
            request,
            root_name,
            canonical_root.to_path_buf(),
        );
        let bytes = serde_json::to_vec(&receipt)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_RECEIPT_BYTES,
            "provisioning receipt is too large"
        );
        let mut file = private_new_file(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        sync_directory(&self.directory)?;
        Ok(receipt)
    }

    pub(crate) fn read(&self, operation_id: Uuid) -> Result<Option<ProvisioningReceipt>> {
        self.read_path(&self.path(operation_id))
    }

    pub(crate) fn find_by_session_id(
        &self,
        session_id: &str,
    ) -> Result<Option<ProvisioningReceipt>> {
        self.find_session_receipt(session_id, false)
    }

    pub(crate) fn find_by_preparation_session_id(
        &self,
        session_id: &str,
    ) -> Result<Option<ProvisioningReceipt>> {
        self.find_session_receipt(session_id, true)
    }

    fn find_session_receipt(
        &self,
        session_id: &str,
        preparation: bool,
    ) -> Result<Option<ProvisioningReceipt>> {
        crate::config::validate_session_id(session_id)?;
        self.verify_directory()?;
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("cannot list provisioning receipts"),
        };
        let mut scanned = 0usize;
        let mut found = None;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(id) else {
                continue;
            };
            scanned += 1;
            anyhow::ensure!(
                scanned <= 4096,
                "provisioning receipt lookup exceeds bounded scan"
            );
            if let Some(receipt) = self.read(id)? {
                let owner_id = if preparation {
                    &receipt.preparation_session_id
                } else {
                    &receipt.session_id
                };
                if owner_id == session_id {
                    anyhow::ensure!(found.is_none(), "duplicate managed session identity");
                    found = Some(receipt);
                }
            }
        }
        Ok(found)
    }

    /// Internal root-scoped sessions accept only the immutable task selected
    /// by their provisioning receipt. Public task text cannot acquire this
    /// broader preparation scope merely by discovering its session ID.
    pub(crate) fn authorize_preparation_task(
        receipt: &ProvisioningReceipt,
        session: &crate::config::Session,
        args: &serde_json::Value,
    ) -> Result<()> {
        anyhow::ensure!(
            receipt
                .preparation_owner
                .as_ref()
                .is_some_and(|owner| owner.matches(session))
                && receipt.canonical_root.as_ref() == Some(&session.cwd)
                && fs::canonicalize(&session.cwd)? == session.cwd,
            "managed preparation owner changed"
        );
        let operation = args
            .get("operation_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok());
        let (expected_operation, task, model, effort) =
            if operation == Some(receipt.task_operation_id) {
                (
                    receipt.task_operation_id,
                    crate::workspace_provisioning::delegated_task(receipt)?,
                    receipt.preparation_model.as_deref(),
                    receipt.preparation_effort.as_deref(),
                )
            } else if let Some(attempt) = receipt
                .environment_attempt
                .as_ref()
                .filter(|attempt| operation == Some(attempt.plan.operation_id))
            {
                anyhow::ensure!(
                    attempt.plan.session.matches(session),
                    "preparation scope changed"
                );
                attempt.plan.validate_scope()?;
                (
                    attempt.plan.operation_id,
                    attempt.plan.delegated_task()?,
                    attempt.model.as_deref(),
                    attempt.effort.as_deref(),
                )
            } else {
                anyhow::bail!("managed preparation requires its accepted typed operation");
            };
        anyhow::ensure!(
            operation == Some(expected_operation)
                && args.get("task").and_then(serde_json::Value::as_str) == Some(task.as_str())
                && model.is_some()
                && effort.is_some()
                && args.get("model").and_then(serde_json::Value::as_str) == model
                && args.get("effort").and_then(serde_json::Value::as_str) == effort,
            "managed preparation task differs from its accepted plan"
        );
        Ok(())
    }

    pub(crate) fn list_nonready(&self, limit: usize) -> Result<Vec<ProvisioningReceipt>> {
        anyhow::ensure!(limit <= 256, "provisioning list limit is too large");
        self.verify_directory()?;
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("cannot list provisioning receipts"),
        };
        let mut scanned = 0usize;
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(id) else {
                continue;
            };
            scanned += 1;
            anyhow::ensure!(scanned <= 4096, "provisioning list exceeds bounded scan");
            if let Some(receipt) = self.read(id)?
                && receipt.phase != ProvisioningPhase::WorkspaceReady
            {
                anyhow::ensure!(result.len() < limit, "provisioning list exceeds limit");
                result.push(receipt);
            }
        }
        result.sort_by_key(|receipt| (receipt.accepted_at, receipt.operation_id));
        Ok(result)
    }

    /// A repository has one mutating ensure/fetch owner at a time, including
    /// across process restarts. A new session may claim it only after the old
    /// owner has verified its workspace and pinned base.
    pub(crate) async fn claim_repository(
        &self,
        repository: &RepositoryId,
        operation_id: Uuid,
    ) -> Result<EnsureAdmission> {
        let _lock = crate::config::acquire_session_lifecycle_lock().await?;
        self.ensure_directory()?;
        let path = self.ensure_path(repository);
        match self.read_claim(&path)? {
            Some(claim) if claim.operation_id == operation_id => {
                anyhow::ensure!(
                    claim.repository == *repository,
                    "repository ensure identity mismatch"
                );
                Ok(EnsureAdmission::Owned)
            }
            Some(claim) if claim.phase != EnsurePhase::Ready => Ok(EnsureAdmission::Busy),
            _ => {
                let claim = EnsureClaim {
                    repository: repository.clone(),
                    operation_id,
                    phase: EnsurePhase::Owned,
                };
                self.write_claim(&path, &claim)?;
                Ok(EnsureAdmission::Owned)
            }
        }
    }

    pub(crate) async fn mark_repository_ready(
        &self,
        repository: &RepositoryId,
        operation_id: Uuid,
    ) -> Result<()> {
        self.set_claim_phase(repository, operation_id, EnsurePhase::Ready)
            .await
    }

    pub(crate) async fn mark_repository_uncertain(
        &self,
        repository: &RepositoryId,
        operation_id: Uuid,
    ) -> Result<()> {
        self.set_claim_phase(
            repository,
            operation_id,
            EnsurePhase::ReconciliationRequired,
        )
        .await
    }

    async fn set_claim_phase(
        &self,
        repository: &RepositoryId,
        operation_id: Uuid,
        phase: EnsurePhase,
    ) -> Result<()> {
        let _lock = crate::config::acquire_session_lifecycle_lock().await?;
        let path = self.ensure_path(repository);
        let mut claim = self
            .read_claim(&path)?
            .context("repository ensure claim disappeared")?;
        anyhow::ensure!(
            claim.repository == *repository && claim.operation_id == operation_id,
            "repository ensure ownership changed"
        );
        claim.phase = phase;
        self.write_claim(&path, &claim)
    }

    fn ensure_path(&self, repository: &RepositoryId) -> PathBuf {
        let digest = Sha256::digest(repository.logical_name().as_bytes());
        self.directory.join(format!("ensure-{digest:x}.json"))
    }

    fn read_claim(&self, path: &Path) -> Result<Option<EnsureClaim>> {
        self.verify_directory()?;
        let file = match private_open_file(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("cannot safely open repository ensure claim"),
        };
        anyhow::ensure!(
            file.metadata()?.len() <= MAX_RECEIPT_BYTES,
            "repository ensure claim is too large"
        );
        let mut bytes = Vec::new();
        file.take(MAX_RECEIPT_BYTES + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_RECEIPT_BYTES,
            "repository ensure claim is too large"
        );
        Ok(Some(
            serde_json::from_slice(&bytes).context("invalid repository ensure claim")?,
        ))
    }

    fn write_claim(&self, path: &Path, claim: &EnsureClaim) -> Result<()> {
        let temporary = self
            .directory
            .join(format!("ensure-{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = private_new_file(&temporary)?;
            file.write_all(&serde_json::to_vec(claim)?)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            sync_directory(&self.directory)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(crate) async fn update(
        &self,
        operation_id: Uuid,
        mutate: impl FnOnce(&mut ProvisioningReceipt) -> Result<()>,
    ) -> Result<ProvisioningReceipt> {
        let _lock = crate::config::acquire_session_lifecycle_lock().await?;
        let path = self.path(operation_id);
        let mut receipt = self
            .read_path(&path)?
            .context("managed provisioning receipt disappeared")?;
        mutate(&mut receipt)?;
        let bytes = serde_json::to_vec(&receipt)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_RECEIPT_BYTES,
            "provisioning receipt is too large"
        );
        let temporary = self
            .directory
            .join(format!("{}.{}.tmp", operation_id, Uuid::new_v4()));
        let result = (|| {
            let mut file = private_new_file(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            sync_directory(&self.directory)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(receipt)
    }

    fn path(&self, operation_id: Uuid) -> PathBuf {
        self.directory.join(format!("{operation_id}.json"))
    }

    fn ensure_directory(&self) -> Result<()> {
        let parent = self
            .directory
            .parent()
            .context("provisioning store has no parent")?;
        fs::create_dir_all(parent)?;
        let parent = fs::canonicalize(parent)?;
        anyhow::ensure!(
            parent == self.directory.parent().unwrap(),
            "provisioning state parent is not canonical"
        );
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "provisioning store must be a real directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&self.directory)?
            }
            Err(error) => return Err(error.into()),
        }
        anyhow::ensure!(
            fs::canonicalize(&self.directory)? == self.directory,
            "provisioning store changed"
        );
        Ok(())
    }

    fn read_path(&self, path: &Path) -> Result<Option<ProvisioningReceipt>> {
        self.verify_directory()?;
        let file = match private_open_file(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("cannot safely open provisioning receipt"),
        };
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() <= MAX_RECEIPT_BYTES,
            "invalid provisioning receipt file"
        );
        let mut bytes = Vec::new();
        file.take(MAX_RECEIPT_BYTES + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_RECEIPT_BYTES,
            "provisioning receipt is too large"
        );
        let receipt: ProvisioningReceipt =
            serde_json::from_slice(&bytes).context("invalid provisioning receipt")?;
        anyhow::ensure!(
            receipt.version == RECEIPT_VERSION,
            "unsupported provisioning receipt version"
        );
        anyhow::ensure!(
            path.file_stem().and_then(|name| name.to_str())
                == Some(&receipt.operation_id.to_string()),
            "provisioning receipt identity mismatch"
        );
        receipt.request.validate()?;
        receipt.validate_identity()?;
        Ok(Some(receipt))
    }

    fn verify_directory(&self) -> Result<()> {
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "provisioning store must be a real directory"
                );
                anyhow::ensure!(
                    fs::canonicalize(&self.directory)? == self.directory,
                    "provisioning store changed"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

fn private_new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

fn private_open_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ManagedRequest {
        ManagedRequest {
            repository: RepositoryId::parse("F4AH6O/Temote-MCP.git", "github.com").unwrap(),
            base: Some("main".to_owned()),
            vcs: VcsPreference::Auto,
        }
    }

    #[test]
    fn preparation_authority_binds_plan_and_full_owner() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(fixture.path()).unwrap();
        let mut receipt = ProvisioningReceipt::new(Uuid::new_v4(), request(), "src", root.clone());
        let session = crate::config::Session {
            id: receipt.preparation_session_id.clone(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 1,
            process_id: 2,
            permission_mode: crate::config::PermissionMode::Agent,
            grants: crate::config::SessionGrants::default(),
        };
        receipt.preparation_owner = Some(ActivatedSessionInstance::from_session(
            &session,
            receipt.task_operation_id,
        ));
        receipt.preparation_model = Some("checked-model".into());
        receipt.preparation_effort = Some("medium".into());
        let args = serde_json::json!({
            "operation_id": receipt.task_operation_id,
            "task": crate::workspace_provisioning::delegated_task(&receipt).unwrap(),
            "model": "checked-model", "effort": "medium",
        });
        crate::test_support::run(0x5052_4550_4155_5448, 256, |ctx| {
            let changes = noprop::sample_usize_in(ctx, 0..=255);
            let mut current = session.clone();
            let mut input = args.clone();
            if changes & 1 != 0 {
                input["operation_id"] = serde_json::json!(Uuid::new_v4());
            }
            if changes & 2 != 0 {
                input["task"] = serde_json::json!("arbitrary task");
            }
            if changes & 4 != 0 {
                input["model"] = serde_json::json!("other-model");
            }
            if changes & 8 != 0 {
                input["effort"] = serde_json::json!("high");
            }
            if changes & 16 != 0 {
                current.started_at += 1;
            }
            if changes & 32 != 0 {
                current.process_id += 1;
            }
            if changes & 64 != 0 {
                current.permission_mode = crate::config::PermissionMode::Ask;
            }
            if changes & 128 != 0 {
                current.grants.ambient_git_credentials = true;
            }
            assert_eq!(
                RepositoryStore::authorize_preparation_task(&receipt, &current, &input).is_ok(),
                changes == 0
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn accepted_retry_preserves_identity_and_rejects_changed_request() {
        let fixture = tempfile::tempdir().unwrap();
        let store = RepositoryStore::at(
            fs::canonicalize(fixture.path())
                .unwrap()
                .join("state")
                .join("managed-provisioning"),
        );
        let operation_id = Uuid::new_v4();
        let root = fs::canonicalize(fixture.path()).unwrap();
        let first = store
            .accept(operation_id, request(), "src", &root)
            .await
            .unwrap();
        let replay = store
            .accept(operation_id, request(), "src", &root)
            .await
            .unwrap();
        assert_eq!(first.session_id, replay.session_id);
        assert_eq!(first.workspace_id, replay.workspace_id);
        assert_eq!(first.change_id, replay.change_id);
        let mut changed = request();
        changed.base = Some("other".to_owned());
        assert!(
            store
                .accept(operation_id, changed, "src", &root)
                .await
                .unwrap_err()
                .to_string()
                .contains("operation_conflict")
        );
    }

    #[tokio::test]
    async fn receipt_survives_update_and_rejects_symlink() {
        let fixture = tempfile::tempdir().unwrap();
        let store = RepositoryStore::at(
            fs::canonicalize(fixture.path())
                .unwrap()
                .join("state")
                .join("managed-provisioning"),
        );
        let id = Uuid::new_v4();
        let root = fs::canonicalize(fixture.path()).unwrap();
        store.accept(id, request(), "src", &root).await.unwrap();
        store
            .update(id, |receipt| {
                receipt.phase = ProvisioningPhase::RepositoryPreparing;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            store.read(id).unwrap().unwrap().phase,
            ProvisioningPhase::RepositoryPreparing
        );
        #[cfg(unix)]
        {
            let link_id = Uuid::new_v4();
            std::os::unix::fs::symlink(store.path(id), store.path(link_id)).unwrap();
            assert!(store.read(link_id).is_err());
        }
    }

    #[tokio::test]
    async fn repository_ensure_serializes_independent_operations() {
        let fixture = tempfile::tempdir().unwrap();
        let store = RepositoryStore::at(
            fs::canonicalize(fixture.path())
                .unwrap()
                .join("state")
                .join("managed-provisioning"),
        );
        let repository = request().repository;
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        assert_eq!(
            store.claim_repository(&repository, first).await.unwrap(),
            EnsureAdmission::Owned
        );
        assert_eq!(
            store.claim_repository(&repository, first).await.unwrap(),
            EnsureAdmission::Owned
        );
        assert_eq!(
            store.claim_repository(&repository, second).await.unwrap(),
            EnsureAdmission::Busy
        );
        store
            .mark_repository_ready(&repository, first)
            .await
            .unwrap();
        assert_eq!(
            store.claim_repository(&repository, second).await.unwrap(),
            EnsureAdmission::Owned
        );
        store
            .mark_repository_uncertain(&repository, second)
            .await
            .unwrap();
        assert_eq!(
            store.claim_repository(&repository, first).await.unwrap(),
            EnsureAdmission::Busy
        );
    }

    #[test]
    fn activated_owner_fences_scope_and_mode() {
        let fixture = tempfile::tempdir().unwrap();
        let mut session = crate::config::new_session_with_mode(
            fixture.path(),
            Some("managed-owner"),
            crate::config::PermissionMode::Agent,
        )
        .unwrap();
        session.process_id = 42;
        let owner = ActivatedSessionInstance::from_session(&session, Uuid::new_v4());
        assert!(owner.matches(&session));
        session.permission_mode = crate::config::PermissionMode::Ask;
        assert!(!owner.matches(&session));
        session.permission_mode = crate::config::PermissionMode::Agent;
        session
            .permitted_directories
            .push(fixture.path().join("other"));
        assert!(!owner.matches(&session));
    }
}
