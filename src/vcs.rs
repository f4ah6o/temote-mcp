//! Internal VCS transaction core for Temote-managed workspaces.
//!
//! V3 has one real backend: Jujutsu. Git is an explicit compatibility
//! placeholder. Callers select typed operations and never supply raw argv.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;
const REGISTRY_DIR: &str = ".temote-vcs";
const WORKSPACES_DIR: &str = "workspaces";
const OPERATIONS_DIR: &str = "operations";
const LOCK_FILE: &str = ".lock";
const MAX_RECORD_BYTES: usize = 128 * 1024;
const MAX_WORKSPACE_ID_BYTES: usize = 64;
const JJ_REVISION_TEMPLATE: &str =
    "change_id ++ \"\\n\" ++ commit_id ++ \"\\n\" ++ conflict ++ \"\\n\" ++ empty ++ \"\\n\"";
const JJ_OPERATION_TEMPLATE: &str = "self.id().short() ++ \"\\n\"";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VcsBackendKind {
    Jujutsu,
    Git,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Capability {
    pub state: CapabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Capability {
    fn supported(version: Option<String>, detail: Option<String>) -> Self {
        Self {
            state: CapabilityState::Supported,
            observed_version: version,
            detail,
        }
    }

    fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            state: CapabilityState::Unsupported,
            observed_version: None,
            detail: Some(detail.into()),
        }
    }

    fn unknown(detail: impl Into<String>) -> Self {
        Self {
            state: CapabilityState::Unknown,
            observed_version: None,
            detail: Some(detail.into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct VcsCapabilities {
    pub preferred_backend: VcsBackendKind,
    pub jj: Capability,
    pub git_binary: Capability,
    pub git_backend: Capability,
    pub shallow_clone: Capability,
    pub submodules: Capability,
    pub git_lfs: Capability,
    pub required_git_hooks: Capability,
    pub colocated_git: Capability,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceEnsureRequest {
    pub workspace_id: String,
    pub task_id: String,
    pub backend: VcsBackendKind,
    pub base_revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct WorkspaceRecord {
    schema_version: u32,
    request_fingerprint: String,
    repository_root: PathBuf,
    workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    backend: VcsBackendKind,
    path: PathBuf,
    base_revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct JujutsuState {
    pub logical_change_id: String,
    pub materialized_revision: String,
    pub vcs_operation_id: String,
    pub conflicted: bool,
    pub empty: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceState {
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub backend: VcsBackendKind,
    pub path: PathBuf,
    pub base_revision: String,
    pub jj: JujutsuState,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceEnsureResult {
    pub created: bool,
    pub workspace: WorkspaceState,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct VcsSnapshotObserved {
    pub operation_id: Uuid,
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    pub backend: VcsBackendKind,
    pub logical_change_id: String,
    pub before_revision: String,
    pub after_revision: String,
    pub vcs_operation_id: String,
    pub conflicted: bool,
    pub empty: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotResult {
    pub operation_id: Uuid,
    pub replayed: bool,
    pub reconciled: bool,
    pub before: JujutsuState,
    pub after: JujutsuState,
    pub observation: VcsSnapshotObserved,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReceiptState {
    Accepted,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SnapshotReceipt {
    schema_version: u32,
    operation_id: Uuid,
    request_fingerprint: String,
    state: ReceiptState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<JujutsuState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<SnapshotResult>,
    #[serde(default)]
    observation_pending: bool,
    #[serde(default)]
    observation_base_revision: Option<u64>,
    #[serde(default)]
    delegated: bool,
}

/// The bounded, typed result of a server-owned delegated jj snapshot task.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DelegatedSnapshotReport {
    pub operation_id: Uuid,
    pub task_id: String,
    pub workspace_id: String,
    pub execution_id: Option<String>,
    pub before: JujutsuState,
    pub after: JujutsuState,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VcsErrorCode {
    InvalidRequest,
    BackendUnsupported,
    CapabilityUnsupported,
    WorkspaceConflict,
    WorkspaceUnregistered,
    WorkspaceMissing,
    CommandFailed,
    InvalidBackendOutput,
    OperationConflict,
    ReconciliationRequired,
    Io,
}

impl VcsErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::BackendUnsupported => "backend_unsupported",
            Self::CapabilityUnsupported => "capability_unsupported",
            Self::WorkspaceConflict => "workspace_conflict",
            Self::WorkspaceUnregistered => "workspace_unregistered",
            Self::WorkspaceMissing => "workspace_missing",
            Self::CommandFailed => "command_failed",
            Self::InvalidBackendOutput => "invalid_backend_output",
            Self::OperationConflict => "operation_conflict",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::Io => "io_error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VcsError {
    pub code: VcsErrorCode,
    pub message: String,
}

impl VcsError {
    fn new(code: VcsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for VcsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for VcsError {}

type VcsResult<T> = Result<T, VcsError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    stdout: String,
    stderr: String,
    success: bool,
}

pub(crate) trait CommandRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> std::io::Result<CommandOutput>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> std::io::Result<CommandOutput> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GIT_TERMINAL_PROMPT", "0");
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output()?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            success: output.status.success(),
        })
    }
}

pub(crate) trait VcsObservationSink: Send + Sync {
    fn checkpoint(&self) -> VcsResult<Option<u64>> {
        Ok(None)
    }
    fn record(
        &self,
        observation: &VcsSnapshotObserved,
        base_revision: Option<u64>,
    ) -> VcsResult<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoopObservationSink;

impl VcsObservationSink for NoopObservationSink {
    fn record(
        &self,
        _observation: &VcsSnapshotObserved,
        _base_revision: Option<u64>,
    ) -> VcsResult<()> {
        Ok(())
    }
}

pub(crate) struct JournalVcsObservationSink {
    session: crate::config::Session,
    store: crate::observation::ObservationStore,
}

impl JournalVcsObservationSink {
    pub(crate) fn new(session: crate::config::Session) -> VcsResult<Self> {
        let store = crate::observation::ObservationStore::default_store().map_err(|e| {
            VcsError::new(
                VcsErrorCode::Io,
                format!("open VCS observation journal: {e}"),
            )
        })?;
        Ok(Self { session, store })
    }
}

impl VcsObservationSink for JournalVcsObservationSink {
    fn checkpoint(&self) -> VcsResult<Option<u64>> {
        let status = self.store.status(&self.session.id).map_err(|e| {
            VcsError::new(
                VcsErrorCode::Io,
                format!("read VCS observation checkpoint: {e}"),
            )
        })?;
        Ok(Some(status.base_revision))
    }

    fn record(&self, event: &VcsSnapshotObserved, base_revision: Option<u64>) -> VcsResult<()> {
        use crate::observation::{
            ActorRef, OBSERVATION_SCHEMA_VERSION, Observation, ObservationContent, ObservationKind,
            Provenance, SessionInstanceRef, StateRef, TargetRef,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| VcsError::new(VcsErrorCode::Io, format!("observation clock: {e}")))?
            .as_secs();
        let observation = Observation {
            id: event.operation_id,
            schema_version: OBSERVATION_SCHEMA_VERSION,
            observed_at: now,
            accepted_at: None,
            session_id: self.session.id.clone(),
            session_instance: SessionInstanceRef {
                started_at: self.session.started_at,
                process_id: self.session.process_id,
            },
            repository: crate::observation::repository_label(&self.session),
            repository_key: crate::observation::repository::repository_key_for_workspace(
                &self.session.cwd,
            ),
            workspace_id: Some(event.workspace_id.clone()),
            task_id: event.task_id.clone(),
            execution_id: event.execution_id.clone(),
            operation_id: Some(event.operation_id.to_string()),
            actor: ActorRef {
                transport: "vcs".into(),
                principal: None,
            },
            target: TargetRef {
                backend: "jujutsu".into(),
            },
            action: "vcs_snapshot".into(),
            kind: ObservationKind::ExecutionState,
            content: ObservationContent::View {
                view: serde_json::json!({
                    "logical_change_id": event.logical_change_id,
                    "before_revision": event.before_revision,
                    "after_revision": event.after_revision,
                    "vcs_operation_id": event.vcs_operation_id,
                    "conflicted": event.conflicted,
                    "empty": event.empty,
                }),
            },
            state_ref: Some(StateRef {
                task_id: event.task_id.clone(),
                ..StateRef::default()
            }),
            evidence_refs: Vec::new(),
            provenance: Provenance {
                tool: "vcs_snapshot".into(),
                source: "vcs_receipt".into(),
                control_action: None,
            },
            revision: 0,
            dedupe_key: format!("vcs-snapshot:{}", event.operation_id),
        };
        if let Some(existing) = self
            .store
            .get(&self.session.id, event.operation_id)
            .map_err(|e| VcsError::new(VcsErrorCode::Io, format!("read VCS observation: {e}")))?
        {
            if existing.dedupe_key == observation.dedupe_key
                && existing.session_instance.started_at == observation.session_instance.started_at
                && existing.session_instance.process_id == observation.session_instance.process_id
                && existing.task_id == event.task_id
                && existing.execution_id == event.execution_id
                && existing.workspace_id == observation.workspace_id
                && serde_json::to_value(&existing.content).ok()
                    == serde_json::to_value(&observation.content).ok()
            {
                return Ok(());
            }
            return Err(VcsError::new(
                VcsErrorCode::ReconciliationRequired,
                "VCS observation identity conflict",
            ));
        }
        let current_base = self.checkpoint()?.unwrap_or(0);
        if base_revision.is_none_or(|base| current_base > base) {
            return Err(VcsError::new(
                VcsErrorCode::ReconciliationRequired,
                "VCS journal retention crossed pending receipt; append uniqueness is unknown",
            ));
        }
        self.store
            .append(observation)
            .map_err(|e| VcsError::new(VcsErrorCode::Io, format!("append VCS observation: {e}")))?;
        Ok(())
    }
}

pub(crate) struct VcsManager<R = SystemCommandRunner, S = NoopObservationSink> {
    repository_root: PathBuf,
    managed_root: PathBuf,
    store_root: PathBuf,
    runner: R,
    observation_sink: S,
}

impl VcsManager<SystemCommandRunner, NoopObservationSink> {
    pub(crate) fn open(repository_root: &Path, managed_root: &Path) -> VcsResult<Self> {
        Self::with_components(
            repository_root,
            managed_root,
            SystemCommandRunner,
            NoopObservationSink,
        )
    }
}

impl VcsManager<SystemCommandRunner, JournalVcsObservationSink> {
    pub(crate) fn open_for_session(
        repository_root: &Path,
        managed_root: &Path,
        session: &crate::config::Session,
    ) -> VcsResult<Self> {
        let sink = JournalVcsObservationSink::new(session.clone())?;
        Self::with_observation_sink(repository_root, managed_root, SystemCommandRunner, sink)
    }
}

impl<R: CommandRunner, S: VcsObservationSink> VcsManager<R, S> {
    pub(crate) fn with_observation_sink(
        repository_root: &Path,
        managed_root: &Path,
        runner: R,
        sink: S,
    ) -> VcsResult<Self> {
        Self::with_components(repository_root, managed_root, runner, sink)
    }

    fn with_components(
        repository_root: &Path,
        managed_root: &Path,
        runner: R,
        observation_sink: S,
    ) -> VcsResult<Self> {
        let repository_root = canonical_existing_directory(repository_root, "repository root")?;
        let managed_root = canonical_existing_directory(managed_root, "managed workspace root")?;
        let store_root = managed_root.join(REGISTRY_DIR);
        ensure_private_directory(&store_root)?;
        ensure_private_directory(&store_root.join(WORKSPACES_DIR))?;
        ensure_private_directory(&store_root.join(OPERATIONS_DIR))?;
        Ok(Self {
            repository_root,
            managed_root,
            store_root,
            runner,
            observation_sink,
        })
    }

    pub(crate) fn capabilities(&self) -> VcsCapabilities {
        let jj = tool_capability(&self.runner, "jj", &["--version"]);
        let git_binary = tool_capability(&self.runner, "git", &["--version"]);
        let shallow_clone = if jj.state == CapabilityState::Supported {
            match self.runner.run(
                "jj",
                &strings(&["git", "clone", "--help"]),
                Some(&self.repository_root),
            ) {
                Ok(output) if output.success && output.stdout.contains("--depth") => {
                    Capability::supported(None, Some("jj git clone advertises --depth".to_owned()))
                }
                Ok(output) if output.success => {
                    Capability::unknown("installed jj help does not advertise --depth")
                }
                Ok(output) => Capability::unknown(format!(
                    "cannot inspect jj shallow-clone capability: {}",
                    bounded_detail(&output.stderr)
                )),
                Err(error) => Capability::unknown(format!(
                    "cannot inspect jj shallow-clone capability: {error}"
                )),
            }
        } else {
            Capability::unknown("jj is unavailable")
        };
        let git_lfs = match self.runner.run(
            "git-lfs",
            &strings(&["--version"]),
            Some(&self.repository_root),
        ) {
            Ok(output) if output.success => Capability::supported(
                first_nonempty_line(&output.stdout).map(str::to_owned),
                Some(
                    "git-lfs executable is available; repository usage is not inferred".to_owned(),
                ),
            ),
            Ok(_) | Err(_) => Capability::unknown(
                "git-lfs executable was not positively detected; repository requirement is unknown",
            ),
        };

        VcsCapabilities {
            preferred_backend: VcsBackendKind::Jujutsu,
            jj,
            git_binary,
            git_backend: Capability::unsupported(
                "Git compatibility backend is intentionally not implemented in V3",
            ),
            shallow_clone,
            submodules: Capability::unknown(
                "submodule compatibility is capability-gated until repository acceptance is implemented",
            ),
            git_lfs,
            required_git_hooks: Capability::unknown(
                "required Git hook compatibility is not inferred from repository contents",
            ),
            colocated_git: Capability::supported(
                None,
                Some(
                    "supported with caveats: a jj working copy may not expose a normal Git HEAD before a Git ref exists"
                        .to_owned(),
                ),
            ),
        }
    }

    pub(crate) fn workspace_ensure(
        &self,
        request: &WorkspaceEnsureRequest,
    ) -> VcsResult<WorkspaceEnsureResult> {
        validate_workspace_id(&request.workspace_id)?;
        validate_correlation_id("task_id", &request.task_id)?;
        validate_exact_revision(&request.base_revision)?;
        self.require_backend(request.backend)?;
        self.require_jj_repository()?;

        let fingerprint = workspace_fingerprint(&self.repository_root, request);
        let workspace_path = self.managed_root.join(&request.workspace_id);
        let record_path = self.workspace_record_path(&request.workspace_id);
        let _lock = self.acquire_store_lock()?;

        if record_path.exists() {
            let record: WorkspaceRecord = read_json_record(&record_path)?;
            validate_workspace_record(&record, &self.repository_root, &self.managed_root)?;
            if record.request_fingerprint != fingerprint {
                return Err(VcsError::new(
                    VcsErrorCode::WorkspaceConflict,
                    format!(
                        "workspace {} exists with a different normalized request",
                        request.workspace_id
                    ),
                ));
            }
            return Ok(WorkspaceEnsureResult {
                created: false,
                workspace: self.inspect_record(&record)?,
            });
        }

        if workspace_path.exists() {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceUnregistered,
                format!(
                    "workspace path exists without a Temote registry record: {}",
                    workspace_path.display()
                ),
            ));
        }

        let workspace_path_utf8 = workspace_path.to_str().ok_or_else(|| {
            VcsError::new(
                VcsErrorCode::InvalidRequest,
                "managed workspace path is not valid UTF-8",
            )
        })?;
        let args = strings(&[
            "workspace",
            "add",
            "--name",
            &request.workspace_id,
            "-r",
            &request.base_revision,
            workspace_path_utf8,
        ]);
        self.run_checked(
            "jj",
            &args,
            Some(&self.repository_root),
            "create jj workspace",
        )?;

        let canonical_path = canonical_existing_directory(&workspace_path, "jj workspace")?;
        ensure_descendant(&self.managed_root, &canonical_path, "jj workspace")?;
        let record = WorkspaceRecord {
            schema_version: SCHEMA_VERSION,
            request_fingerprint: fingerprint,
            repository_root: self.repository_root.clone(),
            workspace_id: request.workspace_id.clone(),
            task_id: Some(request.task_id.clone()),
            backend: request.backend,
            path: canonical_path,
            base_revision: request.base_revision.clone(),
        };
        write_json_atomic(&record_path, &record)?;
        Ok(WorkspaceEnsureResult {
            created: true,
            workspace: self.inspect_record(&record)?,
        })
    }

    pub(crate) fn inspect(&self, workspace_id: &str) -> VcsResult<WorkspaceState> {
        validate_workspace_id(workspace_id)?;
        let record_path = self.workspace_record_path(workspace_id);
        if !record_path.exists() {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceMissing,
                format!("workspace {workspace_id} is not registered"),
            ));
        }
        let record: WorkspaceRecord = read_json_record(&record_path)?;
        validate_workspace_record(&record, &self.repository_root, &self.managed_root)?;
        self.inspect_record(&record)
    }

    /// Index an already provisioned, marker-validated managed workspace for
    /// task-correlated snapshots. This writes only VCS registry metadata; the
    /// provisioning task remains the sole creator of the workspace.
    pub(crate) fn register_provisioned_snapshot_workspace(
        &self,
        receipt: &crate::repository_store::ProvisioningReceipt,
        task_id: &str,
    ) -> VcsResult<()> {
        if receipt.phase != crate::repository_store::ProvisioningPhase::WorkspaceReady {
            return Err(VcsError::new(
                VcsErrorCode::InvalidRequest,
                "provisioned workspace is not ready",
            ));
        }
        let root = receipt.canonical_root.as_deref().ok_or_else(|| {
            VcsError::new(VcsErrorCode::InvalidRequest, "provisioned root is missing")
        })?;
        let observed_base =
            crate::workspace_provisioning::inspect_ready(root, receipt).map_err(|error| {
                VcsError::new(
                    VcsErrorCode::WorkspaceConflict,
                    format!("provisioned workspace marker invalid: {error}"),
                )
            })?;
        let workspace_id = receipt.workspace_id.to_string();
        validate_workspace_id(&workspace_id)?;
        validate_correlation_id("task_id", task_id)?;
        let base_revision = receipt.pinned_base.as_deref().ok_or_else(|| {
            VcsError::new(
                VcsErrorCode::InvalidRequest,
                "provisioned workspace has no pinned base",
            )
        })?;
        if observed_base != base_revision {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceConflict,
                "provisioned pinned base differs from ready marker",
            ));
        }
        validate_exact_revision(base_revision)?;
        self.require_jj_repository()?;
        let workspace_path = canonical_existing_directory(
            &self.managed_root.join(&workspace_id),
            "provisioned workspace",
        )?;
        ensure_descendant(&self.managed_root, &workspace_path, "provisioned workspace")?;
        if workspace_path != self.repository_root {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceConflict,
                "provisioned workspace differs from session repository root",
            ));
        }
        let request = WorkspaceEnsureRequest {
            workspace_id: workspace_id.clone(),
            task_id: task_id.to_owned(),
            backend: VcsBackendKind::Jujutsu,
            base_revision: base_revision.to_owned(),
        };
        let fingerprint = workspace_fingerprint(&self.repository_root, &request);
        let _lock = self.acquire_store_lock()?;
        let path = self.workspace_record_path(&workspace_id);
        if path.exists() {
            let existing: WorkspaceRecord = read_json_record(&path)?;
            validate_workspace_record(&existing, &self.repository_root, &self.managed_root)?;
            if existing.request_fingerprint != fingerprint || existing.path != workspace_path {
                return Err(VcsError::new(
                    VcsErrorCode::WorkspaceConflict,
                    "provisioned workspace registry identity conflict",
                ));
            }
            return Ok(());
        }
        write_json_atomic(
            &path,
            &WorkspaceRecord {
                schema_version: SCHEMA_VERSION,
                request_fingerprint: fingerprint,
                repository_root: self.repository_root.clone(),
                workspace_id,
                task_id: Some(task_id.to_owned()),
                backend: VcsBackendKind::Jujutsu,
                path: workspace_path,
                base_revision: base_revision.to_owned(),
            },
        )
    }

    /// Reserve exactly one delegated snapshot. The Accepted receipt is durable
    /// before the caller starts a helper; an exact retry must read its retained
    /// task and must never dispatch a second helper.
    pub(crate) fn reserve_delegated_snapshot(
        &self,
        workspace_id: &str,
        operation_id: Uuid,
        execution_id: Option<&str>,
    ) -> VcsResult<bool> {
        validate_workspace_id(workspace_id)?;
        if let Some(id) = execution_id {
            validate_correlation_id("execution_id", id)?;
        }
        let _lock = self.acquire_store_lock()?;
        let path = self.operation_receipt_path(operation_id);
        let fingerprint = snapshot_fingerprint(workspace_id, execution_id);
        if path.exists() {
            let receipt: SnapshotReceipt = read_json_record(&path)?;
            validate_receipt(&receipt, operation_id)?;
            if receipt.request_fingerprint != fingerprint
                || receipt.execution_id.as_deref() != execution_id
                || !receipt.delegated
            {
                return Err(VcsError::new(
                    VcsErrorCode::OperationConflict,
                    "delegated snapshot operation correlation changed",
                ));
            }
            return Ok(false);
        }
        let mut scanned = 0usize;
        for entry in std::fs::read_dir(self.store_root.join(OPERATIONS_DIR))
            .map_err(|e| VcsError::new(VcsErrorCode::Io, format!("list VCS receipts: {e}")))?
        {
            let entry = entry.map_err(|e| {
                VcsError::new(VcsErrorCode::Io, format!("read VCS receipt entry: {e}"))
            })?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            scanned += 1;
            if scanned > 4096 {
                return Err(VcsError::new(
                    VcsErrorCode::ReconciliationRequired,
                    "VCS receipt scan exceeds bound",
                ));
            }
            let other: SnapshotReceipt = read_json_record(&entry.path())?;
            let belongs = other.request_fingerprint
                == snapshot_fingerprint(workspace_id, other.execution_id.as_deref())
                || other.request_fingerprint == legacy_snapshot_fingerprint(workspace_id);
            if belongs && (other.state == ReceiptState::Accepted || other.observation_pending) {
                return Err(VcsError::new(
                    VcsErrorCode::ReconciliationRequired,
                    "workspace has another pending VCS receipt",
                ));
            }
        }
        let record: WorkspaceRecord = read_json_record(&self.workspace_record_path(workspace_id))?;
        validate_workspace_record(&record, &self.repository_root, &self.managed_root)?;
        if record.workspace_id != workspace_id
            || record.task_id.is_none()
            || record.backend != VcsBackendKind::Jujutsu
        {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceConflict,
                "delegated snapshot workspace is not task-bound jj",
            ));
        }
        write_json_atomic(
            &path,
            &SnapshotReceipt {
                schema_version: SCHEMA_VERSION,
                operation_id,
                request_fingerprint: fingerprint,
                state: ReceiptState::Accepted,
                task_id: record.task_id,
                execution_id: execution_id.map(str::to_owned),
                before: None,
                result: None,
                observation_pending: false,
                observation_base_revision: self.observation_sink.checkpoint()?,
                delegated: true,
            },
        )?;
        Ok(true)
    }

    /// Import only a completed native report from the retained helper task.
    /// This path performs metadata I/O and observation append, never jj I/O.
    pub(crate) fn import_delegated_snapshot(
        &self,
        workspace_id: &str,
        operation_id: Uuid,
        execution_id: Option<&str>,
        report: &DelegatedSnapshotReport,
    ) -> VcsResult<SnapshotResult> {
        let _lock = self.acquire_store_lock()?;
        let path = self.operation_receipt_path(operation_id);
        let mut receipt: SnapshotReceipt = read_json_record(&path)?;
        validate_receipt(&receipt, operation_id)?;
        if receipt.request_fingerprint != snapshot_fingerprint(workspace_id, execution_id)
            || receipt.execution_id.as_deref() != execution_id
            || !receipt.delegated
            || receipt.task_id.as_deref() != Some(report.task_id.as_str())
            || report.operation_id != operation_id
            || report.workspace_id != workspace_id
            || report.execution_id.as_deref() != execution_id
        {
            return Err(VcsError::new(
                VcsErrorCode::OperationConflict,
                "delegated snapshot report correlation mismatch",
            ));
        }
        let record: WorkspaceRecord = read_json_record(&self.workspace_record_path(workspace_id))?;
        validate_workspace_record(&record, &self.repository_root, &self.managed_root)?;
        if record.workspace_id != workspace_id
            || record.task_id != receipt.task_id
            || record.backend != VcsBackendKind::Jujutsu
        {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceConflict,
                "delegated snapshot registry correlation changed",
            ));
        }
        validate_delegated_state(&report.before)?;
        validate_delegated_state(&report.after)?;
        if report.before.logical_change_id != report.after.logical_change_id
            || report.after.conflicted
        {
            return Err(VcsError::new(
                VcsErrorCode::InvalidBackendOutput,
                "delegated snapshot changed logical identity or is conflicted",
            ));
        }
        if receipt.state == ReceiptState::Completed {
            self.flush_observation(&path, &mut receipt)?;
            let mut result = receipt.result.ok_or_else(|| {
                VcsError::new(
                    VcsErrorCode::InvalidBackendOutput,
                    "completed snapshot result missing",
                )
            })?;
            if result.before != report.before || result.after != report.after {
                return Err(VcsError::new(
                    VcsErrorCode::OperationConflict,
                    "delegated snapshot result changed",
                ));
            }
            result.replayed = true;
            return Ok(result);
        }
        let observation = VcsSnapshotObserved {
            operation_id,
            workspace_id: workspace_id.to_owned(),
            task_id: receipt.task_id.clone(),
            execution_id: receipt.execution_id.clone(),
            backend: record.backend,
            logical_change_id: report.after.logical_change_id.clone(),
            before_revision: report.before.materialized_revision.clone(),
            after_revision: report.after.materialized_revision.clone(),
            vcs_operation_id: report.after.vcs_operation_id.clone(),
            conflicted: report.after.conflicted,
            empty: report.after.empty,
        };
        let result = SnapshotResult {
            operation_id,
            replayed: false,
            reconciled: false,
            before: report.before.clone(),
            after: report.after.clone(),
            observation,
        };
        receipt.state = ReceiptState::Completed;
        receipt.result = Some(result.clone());
        receipt.observation_pending = true;
        write_json_atomic(&path, &receipt)?;
        self.flush_observation(&path, &mut receipt)?;
        Ok(result)
    }

    /// Replay a completed delegated receipt and flush its observation outbox.
    pub(crate) fn delegated_snapshot_result(
        &self,
        workspace_id: &str,
        operation_id: Uuid,
        execution_id: Option<&str>,
    ) -> VcsResult<Option<SnapshotResult>> {
        let _lock = self.acquire_store_lock()?;
        let path = self.operation_receipt_path(operation_id);
        let mut receipt: SnapshotReceipt = read_json_record(&path)?;
        validate_receipt(&receipt, operation_id)?;
        if receipt.request_fingerprint != snapshot_fingerprint(workspace_id, execution_id)
            || receipt.execution_id.as_deref() != execution_id
            || !receipt.delegated
        {
            return Err(VcsError::new(
                VcsErrorCode::OperationConflict,
                "delegated snapshot receipt correlation mismatch",
            ));
        }
        self.flush_observation(&path, &mut receipt)?;
        Ok(receipt.result.map(|mut result| {
            result.replayed = true;
            result
        }))
    }

    pub(crate) fn snapshot(
        &self,
        workspace_id: &str,
        operation_id: Uuid,
        execution_id: Option<&str>,
    ) -> VcsResult<SnapshotResult> {
        validate_workspace_id(workspace_id)?;
        if let Some(execution_id) = execution_id {
            validate_correlation_id("execution_id", execution_id)?;
        }
        let request_fingerprint = snapshot_fingerprint(workspace_id, execution_id);
        let receipt_path = self.operation_receipt_path(operation_id);
        let _lock = self.acquire_store_lock()?;

        if receipt_path.exists() {
            let mut receipt: SnapshotReceipt = read_json_record(&receipt_path)?;
            validate_receipt(&receipt, operation_id)?;
            let expected_fingerprint =
                if receipt.task_id.is_none() && receipt.execution_id.is_none() {
                    if execution_id.is_some() {
                        return Err(VcsError::new(
                            VcsErrorCode::OperationConflict,
                            "legacy snapshot operation cannot be rebound to an execution",
                        ));
                    }
                    legacy_snapshot_fingerprint(workspace_id)
                } else {
                    request_fingerprint.clone()
                };
            if receipt.request_fingerprint != expected_fingerprint {
                return Err(VcsError::new(
                    VcsErrorCode::OperationConflict,
                    "operation_id was already accepted for a different VCS snapshot request",
                ));
            }
            self.flush_observation(&receipt_path, &mut receipt)?;
            return match (receipt.state, receipt.result) {
                (ReceiptState::Completed, Some(mut result)) => {
                    result.replayed = true;
                    Ok(result)
                }
                (ReceiptState::Accepted, _) => Err(VcsError::new(
                    VcsErrorCode::ReconciliationRequired,
                    "VCS snapshot was accepted but completion is unknown; reconcile before retry",
                )),
                (ReceiptState::Completed, None) => Err(VcsError::new(
                    VcsErrorCode::InvalidBackendOutput,
                    "completed VCS snapshot receipt is missing its result",
                )),
            };
        }

        let record_path = self.workspace_record_path(workspace_id);
        if !record_path.exists() {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceMissing,
                format!("workspace {workspace_id} is not registered"),
            ));
        }
        let record: WorkspaceRecord = read_json_record(&record_path)?;
        validate_workspace_record(&record, &self.repository_root, &self.managed_root)?;
        let before = self.inspect_jj(&record.path)?;

        let accepted = SnapshotReceipt {
            schema_version: SCHEMA_VERSION,
            operation_id,
            request_fingerprint: request_fingerprint.clone(),
            state: ReceiptState::Accepted,
            task_id: record.task_id.clone(),
            execution_id: execution_id.map(str::to_owned),
            before: Some(before.clone()),
            result: None,
            observation_pending: false,
            observation_base_revision: self.observation_sink.checkpoint()?,
            delegated: false,
        };
        write_json_atomic(&receipt_path, &accepted)?;

        self.run_checked(
            "jj",
            &strings(&["--color=never", "--no-pager", "status"]),
            Some(&record.path),
            "snapshot jj working copy",
        )?;
        let after = self.inspect_jj(&record.path)?;
        let observation = VcsSnapshotObserved {
            operation_id,
            workspace_id: workspace_id.to_owned(),
            task_id: record.task_id.clone(),
            execution_id: execution_id.map(str::to_owned),
            backend: record.backend,
            logical_change_id: after.logical_change_id.clone(),
            before_revision: before.materialized_revision.clone(),
            after_revision: after.materialized_revision.clone(),
            vcs_operation_id: after.vcs_operation_id.clone(),
            conflicted: after.conflicted,
            empty: after.empty,
        };
        let result = SnapshotResult {
            operation_id,
            replayed: false,
            reconciled: false,
            before,
            after,
            observation: observation.clone(),
        };
        let mut completed = SnapshotReceipt {
            schema_version: SCHEMA_VERSION,
            operation_id,
            request_fingerprint,
            state: ReceiptState::Completed,
            task_id: record.task_id.clone(),
            execution_id: execution_id.map(str::to_owned),
            before: None,
            result: Some(result.clone()),
            observation_pending: true,
            observation_base_revision: accepted.observation_base_revision,
            delegated: false,
        };
        write_json_atomic(&receipt_path, &completed)?;
        self.flush_observation(&receipt_path, &mut completed)?;
        Ok(result)
    }

    pub(crate) fn reconcile_snapshot(
        &self,
        workspace_id: &str,
        operation_id: Uuid,
    ) -> VcsResult<SnapshotResult> {
        validate_workspace_id(workspace_id)?;
        let receipt_path = self.operation_receipt_path(operation_id);
        let _lock = self.acquire_store_lock()?;

        if !receipt_path.exists() {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceMissing,
                format!("snapshot receipt {operation_id} does not exist"),
            ));
        }

        let mut receipt: SnapshotReceipt = read_json_record(&receipt_path)?;
        validate_receipt(&receipt, operation_id)?;
        let request_fingerprint = if receipt.task_id.is_none() && receipt.execution_id.is_none() {
            legacy_snapshot_fingerprint(workspace_id)
        } else {
            snapshot_fingerprint(workspace_id, receipt.execution_id.as_deref())
        };
        if receipt.request_fingerprint != request_fingerprint {
            return Err(VcsError::new(
                VcsErrorCode::OperationConflict,
                "operation_id was accepted for a different VCS snapshot request",
            ));
        }

        self.flush_observation(&receipt_path, &mut receipt)?;
        if let (ReceiptState::Completed, Some(mut result)) = (receipt.state, receipt.result.clone())
        {
            result.replayed = true;
            return Ok(result);
        }
        if receipt.state == ReceiptState::Completed {
            return Err(VcsError::new(
                VcsErrorCode::InvalidBackendOutput,
                "completed VCS snapshot receipt is missing its result",
            ));
        }

        let before = receipt.before.clone().ok_or_else(|| {
            VcsError::new(
                VcsErrorCode::ReconciliationRequired,
                "accepted VCS snapshot receipt predates persisted before-state; automatic backfill is unsafe",
            )
        })?;
        let task_id = receipt.task_id.clone().ok_or_else(|| {
            VcsError::new(
                VcsErrorCode::ReconciliationRequired,
                "accepted VCS snapshot receipt predates persisted task correlation; automatic backfill is unsafe",
            )
        })?;

        let record_path = self.workspace_record_path(workspace_id);
        if !record_path.exists() {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceMissing,
                format!("workspace {workspace_id} is not registered"),
            ));
        }
        let record: WorkspaceRecord = read_json_record(&record_path)?;
        validate_workspace_record(&record, &self.repository_root, &self.managed_root)?;
        if record.task_id.as_deref() != Some(task_id.as_str()) {
            return Err(VcsError::new(
                VcsErrorCode::WorkspaceConflict,
                "snapshot receipt task correlation does not match the registered workspace",
            ));
        }
        let after = self.inspect_jj(&record.path)?;

        if after == before {
            return Err(VcsError::new(
                VcsErrorCode::ReconciliationRequired,
                "accepted VCS snapshot has no durable state transition to attribute; refusing to replay jj status because later filesystem edits could be folded into the old operation",
            ));
        }
        let operations = self.run_checked(
            "jj",
            &strings(&[
                "--at-op=@",
                "--ignore-working-copy",
                "--color=never",
                "--no-pager",
                "op",
                "log",
                "--no-graph",
                "-n",
                "2",
                "-T",
                JJ_OPERATION_TEMPLATE,
            ]),
            Some(&record.path),
            "inspect jj operation ancestry for snapshot reconciliation",
        )?;
        let recent = operations
            .stdout
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        if recent.len() != 2
            || recent[0] != after.vcs_operation_id
            || recent[1] != before.vcs_operation_id
        {
            return Err(VcsError::new(
                VcsErrorCode::ReconciliationRequired,
                "accepted VCS snapshot crossed ambiguous jj operations; exact result cannot be attributed",
            ));
        }

        let observation = VcsSnapshotObserved {
            operation_id,
            workspace_id: workspace_id.to_owned(),
            task_id: Some(task_id.clone()),
            execution_id: receipt.execution_id.clone(),
            backend: record.backend,
            logical_change_id: after.logical_change_id.clone(),
            before_revision: before.materialized_revision.clone(),
            after_revision: after.materialized_revision.clone(),
            vcs_operation_id: after.vcs_operation_id.clone(),
            conflicted: after.conflicted,
            empty: after.empty,
        };
        let result = SnapshotResult {
            operation_id,
            replayed: false,
            reconciled: true,
            before,
            after,
            observation: observation.clone(),
        };
        let mut completed = SnapshotReceipt {
            schema_version: SCHEMA_VERSION,
            operation_id,
            request_fingerprint,
            state: ReceiptState::Completed,
            task_id: Some(task_id),
            execution_id: receipt.execution_id,
            before: None,
            result: Some(result.clone()),
            observation_pending: true,
            observation_base_revision: receipt.observation_base_revision,
            delegated: false,
        };
        write_json_atomic(&receipt_path, &completed)?;
        self.flush_observation(&receipt_path, &mut completed)?;
        Ok(result)
    }

    fn flush_observation(&self, path: &Path, receipt: &mut SnapshotReceipt) -> VcsResult<()> {
        if !receipt.observation_pending {
            return Ok(());
        }
        let event = &receipt
            .result
            .as_ref()
            .ok_or_else(|| {
                VcsError::new(
                    VcsErrorCode::InvalidBackendOutput,
                    "pending VCS observation has no completed result",
                )
            })?
            .observation;
        self.observation_sink
            .record(event, receipt.observation_base_revision)?;
        receipt.observation_pending = false;
        write_json_atomic(path, receipt)
    }

    pub(crate) fn workspace_has_unreconciled_snapshots(
        &self,
        workspace_id: &str,
    ) -> VcsResult<bool> {
        validate_workspace_id(workspace_id)?;
        let _lock = self.acquire_store_lock()?;
        let mut scanned = 0usize;
        for entry in std::fs::read_dir(self.store_root.join(OPERATIONS_DIR))
            .map_err(|e| VcsError::new(VcsErrorCode::Io, format!("list VCS receipts: {e}")))?
        {
            let entry = entry.map_err(|e| {
                VcsError::new(VcsErrorCode::Io, format!("read VCS receipt entry: {e}"))
            })?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            scanned += 1;
            if scanned > 4096 {
                return Err(VcsError::new(
                    VcsErrorCode::ReconciliationRequired,
                    "VCS receipt scan exceeds bound",
                ));
            }
            let receipt: SnapshotReceipt = read_json_record(&entry.path())?;
            let belongs = receipt.request_fingerprint
                == snapshot_fingerprint(workspace_id, receipt.execution_id.as_deref())
                || receipt.request_fingerprint == legacy_snapshot_fingerprint(workspace_id);
            if belongs && (receipt.state == ReceiptState::Accepted || receipt.observation_pending) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn require_backend(&self, backend: VcsBackendKind) -> VcsResult<()> {
        match backend {
            VcsBackendKind::Jujutsu => {
                let capabilities = self.capabilities();
                if capabilities.jj.state == CapabilityState::Supported {
                    Ok(())
                } else {
                    Err(VcsError::new(
                        VcsErrorCode::CapabilityUnsupported,
                        capabilities
                            .jj
                            .detail
                            .unwrap_or_else(|| "jj is unavailable".to_owned()),
                    ))
                }
            }
            VcsBackendKind::Git => Err(VcsError::new(
                VcsErrorCode::BackendUnsupported,
                "Git compatibility backend is not implemented in V3; no fallback was attempted",
            )),
        }
    }

    fn require_jj_repository(&self) -> VcsResult<()> {
        if self.repository_root.join(".jj").exists() {
            Ok(())
        } else {
            Err(VcsError::new(
                VcsErrorCode::CapabilityUnsupported,
                format!(
                    "repository is not an initialized Jujutsu repository: {}",
                    self.repository_root.display()
                ),
            ))
        }
    }

    fn workspace_record_path(&self, workspace_id: &str) -> PathBuf {
        self.store_root
            .join(WORKSPACES_DIR)
            .join(format!("{workspace_id}.json"))
    }

    fn operation_receipt_path(&self, operation_id: Uuid) -> PathBuf {
        self.store_root
            .join(OPERATIONS_DIR)
            .join(format!("{operation_id}.json"))
    }

    fn inspect_record(&self, record: &WorkspaceRecord) -> VcsResult<WorkspaceState> {
        if record.backend != VcsBackendKind::Jujutsu {
            return Err(VcsError::new(
                VcsErrorCode::BackendUnsupported,
                "registered workspace backend is not implemented in V3",
            ));
        }
        let path = canonical_existing_directory(&record.path, "registered workspace")?;
        ensure_descendant(&self.managed_root, &path, "registered workspace")?;
        Ok(WorkspaceState {
            workspace_id: record.workspace_id.clone(),
            task_id: record.task_id.clone(),
            backend: record.backend,
            path,
            base_revision: record.base_revision.clone(),
            jj: self.inspect_jj(&record.path)?,
        })
    }

    fn inspect_jj(&self, workspace: &Path) -> VcsResult<JujutsuState> {
        let revision = self.run_checked(
            "jj",
            &strings(&[
                "--ignore-working-copy",
                "--color=never",
                "--no-pager",
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                JJ_REVISION_TEMPLATE,
            ]),
            Some(workspace),
            "inspect jj working-copy revision",
        )?;
        let fields = revision.stdout.lines().map(str::trim).collect::<Vec<_>>();
        if fields.len() != 4 || fields.iter().any(|field| field.is_empty()) {
            return Err(VcsError::new(
                VcsErrorCode::InvalidBackendOutput,
                format!(
                    "unexpected jj revision template output: {:?}",
                    bounded_detail(&revision.stdout)
                ),
            ));
        }
        let conflicted = parse_bool(fields[2], "jj conflict state")?;
        let empty = parse_bool(fields[3], "jj empty state")?;
        let operation = self.run_checked(
            "jj",
            &strings(&[
                "--ignore-working-copy",
                "--color=never",
                "--no-pager",
                "op",
                "log",
                "--no-graph",
                "-n",
                "1",
                "-T",
                JJ_OPERATION_TEMPLATE,
            ]),
            Some(workspace),
            "inspect jj operation",
        )?;
        let vcs_operation_id = first_nonempty_line(&operation.stdout)
            .ok_or_else(|| {
                VcsError::new(
                    VcsErrorCode::InvalidBackendOutput,
                    "jj operation template returned no operation id",
                )
            })?
            .trim()
            .to_owned();

        Ok(JujutsuState {
            logical_change_id: fields[0].to_owned(),
            materialized_revision: fields[1].to_owned(),
            vcs_operation_id,
            conflicted,
            empty,
        })
    }

    fn run_checked(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        label: &str,
    ) -> VcsResult<CommandOutput> {
        let output = self.runner.run(program, args, cwd).map_err(|error| {
            let code = if error.kind() == std::io::ErrorKind::NotFound {
                VcsErrorCode::CapabilityUnsupported
            } else {
                VcsErrorCode::Io
            };
            VcsError::new(code, format!("{label}: {error}"))
        })?;
        if output.success {
            Ok(output)
        } else {
            Err(VcsError::new(
                VcsErrorCode::CommandFailed,
                format!("{label}: {}", bounded_detail(&output.stderr)),
            ))
        }
    }

    fn acquire_store_lock(&self) -> VcsResult<StoreLock> {
        let path = self.store_root.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let file = options
            .open(&path)
            .map_err(|error| VcsError::new(VcsErrorCode::Io, format!("open VCS lock: {error}")))?;
        #[cfg(unix)]
        {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result != 0 {
                return Err(VcsError::new(
                    VcsErrorCode::Io,
                    format!("lock VCS registry: {}", std::io::Error::last_os_error()),
                ));
            }
        }
        Ok(StoreLock { file })
    }
}

struct StoreLock {
    file: File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn tool_capability<R: CommandRunner>(runner: &R, program: &str, args: &[&str]) -> Capability {
    match runner.run(program, &strings(args), None) {
        Ok(output) if output.success => {
            Capability::supported(first_nonempty_line(&output.stdout).map(str::to_owned), None)
        }
        Ok(output) => Capability::unsupported(format!(
            "{program} probe failed: {}",
            bounded_detail(&output.stderr)
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Capability::unsupported(format!("{program} executable was not found"))
        }
        Err(error) => Capability::unknown(format!("{program} probe failed: {error}")),
    }
}

fn canonical_existing_directory(path: &Path, label: &str) -> VcsResult<PathBuf> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("cannot resolve {label} {}: {error}", path.display()),
        )
    })?;
    let metadata = std::fs::symlink_metadata(&canonical).map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("cannot inspect {label} {}: {error}", canonical.display()),
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(VcsError::new(
            VcsErrorCode::InvalidRequest,
            format!("{label} must be a real directory: {}", canonical.display()),
        ));
    }
    Ok(canonical)
}

fn ensure_descendant(root: &Path, candidate: &Path, label: &str) -> VcsResult<()> {
    if candidate.starts_with(root) && candidate != root {
        Ok(())
    } else {
        Err(VcsError::new(
            VcsErrorCode::InvalidRequest,
            format!("{label} escapes managed root {}", root.display()),
        ))
    }
}

fn ensure_private_directory(path: &Path) -> VcsResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            VcsError::new(
                VcsErrorCode::Io,
                format!("create VCS registry parent {}: {error}", parent.display()),
            )
        })?;
    }
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(VcsError::new(
                VcsErrorCode::Io,
                format!("create VCS registry {}: {error}", path.display()),
            ));
        }
    }
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("protect VCS registry {}: {error}", path.display()),
        )
    })?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("inspect VCS registry {}: {error}", path.display()),
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(VcsError::new(
            VcsErrorCode::InvalidRequest,
            format!("VCS registry must be a real directory: {}", path.display()),
        ));
    }
    Ok(())
}

fn validate_workspace_id(workspace_id: &str) -> VcsResult<()> {
    let bytes = workspace_id.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_WORKSPACE_ID_BYTES
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !workspace_id.contains("..");
    if valid {
        Ok(())
    } else {
        Err(VcsError::new(
            VcsErrorCode::InvalidRequest,
            "workspace_id must be 1..=64 bytes, start with ASCII alphanumeric, use only ._- after that, and must not contain '..'",
        ))
    }
}

fn validate_correlation_id(label: &str, value: &str) -> VcsResult<()> {
    const MAX_BYTES: usize = 256;
    let valid =
        !value.is_empty() && value.len() <= MAX_BYTES && !value.chars().any(char::is_control);
    if valid {
        Ok(())
    } else {
        Err(VcsError::new(
            VcsErrorCode::InvalidRequest,
            format!("{label} must be 1..={MAX_BYTES} bytes and contain no control characters"),
        ))
    }
}

fn validate_exact_revision(revision: &str) -> VcsResult<()> {
    if matches!(revision.len(), 40 | 64) && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(VcsError::new(
            VcsErrorCode::InvalidRequest,
            "base_revision must be an exact 40- or 64-character hexadecimal revision id",
        ))
    }
}

fn validate_delegated_state(state: &JujutsuState) -> VcsResult<()> {
    validate_exact_revision(&state.materialized_revision)?;
    let valid = |s: &str| {
        !s.is_empty() && s.len() <= 128 && s.bytes().all(|byte| byte.is_ascii_alphanumeric())
    };
    if !valid(&state.logical_change_id) || !valid(&state.vcs_operation_id) {
        return Err(VcsError::new(
            VcsErrorCode::InvalidBackendOutput,
            "delegated jj identity is invalid",
        ));
    }
    Ok(())
}

fn workspace_fingerprint(repository_root: &Path, request: &WorkspaceEnsureRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"temote-vcs-workspace-v1\0");
    hasher.update(repository_root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(match request.backend {
        VcsBackendKind::Jujutsu => b"jujutsu".as_slice(),
        VcsBackendKind::Git => b"git".as_slice(),
    });
    hasher.update(b"\0");
    hasher.update(request.workspace_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(request.task_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(request.base_revision.as_bytes());
    hex_digest(&hasher.finalize())
}

fn legacy_snapshot_fingerprint(workspace_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"temote-vcs-snapshot-v1\0");
    hasher.update(workspace_id.as_bytes());
    hex_digest(&hasher.finalize())
}

fn snapshot_fingerprint(workspace_id: &str, execution_id: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"temote-vcs-snapshot-v2\0");
    hasher.update(workspace_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(execution_id.unwrap_or_default().as_bytes());
    hex_digest(&hasher.finalize())
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_workspace_record(
    record: &WorkspaceRecord,
    repository_root: &Path,
    managed_root: &Path,
) -> VcsResult<()> {
    if record.schema_version != SCHEMA_VERSION {
        return Err(VcsError::new(
            VcsErrorCode::InvalidBackendOutput,
            "unsupported workspace registry schema",
        ));
    }
    validate_workspace_id(&record.workspace_id)?;
    if let Some(task_id) = record.task_id.as_deref() {
        validate_correlation_id("task_id", task_id)?;
    }
    validate_exact_revision(&record.base_revision)?;
    if record.repository_root != repository_root {
        return Err(VcsError::new(
            VcsErrorCode::WorkspaceConflict,
            "workspace registry belongs to a different repository",
        ));
    }
    ensure_descendant(managed_root, &record.path, "registered workspace")
}

fn validate_receipt(receipt: &SnapshotReceipt, operation_id: Uuid) -> VcsResult<()> {
    if receipt.schema_version != SCHEMA_VERSION || receipt.operation_id != operation_id {
        return Err(VcsError::new(
            VcsErrorCode::InvalidBackendOutput,
            "snapshot receipt identity/schema mismatch",
        ));
    }
    if let Some(task_id) = receipt.task_id.as_deref() {
        validate_correlation_id("task_id", task_id)?;
    }
    if let Some(execution_id) = receipt.execution_id.as_deref() {
        validate_correlation_id("execution_id", execution_id)?;
    }
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> VcsResult<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("serialize VCS record {}: {error}", path.display()),
        )
    })?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(VcsError::new(
            VcsErrorCode::Io,
            "VCS record exceeds size limit",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("VCS record has no parent: {}", path.display()),
        )
    })?;
    ensure_private_directory(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("record"),
        Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(&temporary).map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!(
                "create temporary VCS record {}: {error}",
                temporary.display()
            ),
        )
    })?;
    file.write_all(&bytes).map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("write VCS record {}: {error}", temporary.display()),
        )
    })?;
    file.sync_all().map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("sync VCS record {}: {error}", temporary.display()),
        )
    })?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        VcsError::new(
            VcsErrorCode::Io,
            format!("commit VCS record {}: {error}", path.display()),
        )
    })?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn read_json_record<T: for<'de> Deserialize<'de>>(path: &Path) -> VcsResult<T> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("open VCS record {}: {error}", path.display()),
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        VcsError::new(
            VcsErrorCode::Io,
            format!("inspect VCS record {}: {error}", path.display()),
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES as u64 {
        return Err(VcsError::new(
            VcsErrorCode::InvalidBackendOutput,
            format!("invalid VCS record metadata: {}", path.display()),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            VcsError::new(
                VcsErrorCode::Io,
                format!("read VCS record {}: {error}", path.display()),
            )
        })?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(VcsError::new(
            VcsErrorCode::InvalidBackendOutput,
            "VCS record exceeds size limit",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        VcsError::new(
            VcsErrorCode::InvalidBackendOutput,
            format!("invalid VCS record {}: {error}", path.display()),
        )
    })
}

fn parse_bool(value: &str, label: &str) -> VcsResult<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(VcsError::new(
            VcsErrorCode::InvalidBackendOutput,
            format!("{label} must be true or false, got {value:?}"),
        )),
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn first_nonempty_line(value: &str) -> Option<&str> {
    value.lines().map(str::trim).find(|line| !line.is_empty())
}

fn bounded_detail(value: &str) -> String {
    const LIMIT: usize = 2048;
    let trimmed = value.trim();
    if trimmed.len() <= LIMIT {
        trimmed.to_owned()
    } else {
        trimmed.chars().take(LIMIT).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;

    use super::*;

    #[derive(Clone)]
    struct MockRunner {
        state: Arc<Mutex<MockState>>,
    }

    struct MockState {
        jj_available: bool,
        revisions: VecDeque<(String, String, bool, bool)>,
        operations: VecDeque<String>,
        seen_operations: Vec<String>,
        workspace_adds: usize,
        status_calls: usize,
    }

    impl MockRunner {
        fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockState {
                    jj_available: true,
                    revisions: VecDeque::from([
                        ("change-a".into(), "a".repeat(40), false, false),
                        ("change-a".into(), "a".repeat(40), false, false),
                    ]),
                    operations: VecDeque::from(["op-a".into(), "op-a".into()]),
                    seen_operations: Vec::new(),
                    workspace_adds: 0,
                    status_calls: 0,
                })),
            }
        }

        fn with_revisions(
            self,
            revisions: impl IntoIterator<Item = (String, String, bool, bool)>,
            operations: impl IntoIterator<Item = String>,
        ) -> Self {
            {
                let mut state = self.state.lock().unwrap();
                state.revisions = revisions.into_iter().collect();
                state.operations = operations.into_iter().collect();
            }
            self
        }
    }

    impl CommandRunner for MockRunner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _cwd: Option<&Path>,
        ) -> std::io::Result<CommandOutput> {
            let mut state = self.state.lock().unwrap();
            let words = args.iter().map(String::as_str).collect::<Vec<_>>();
            if program == "jj" && words == ["--version"] {
                if !state.jj_available {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "jj not found",
                    ));
                }
                return Ok(success("jj 0.37.0\n"));
            }
            if program == "git" && words == ["--version"] {
                return Ok(success("git version 2.50.1\n"));
            }
            if program == "git-lfs" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "git-lfs not found",
                ));
            }
            if program == "jj" && words == ["git", "clone", "--help"] {
                return Ok(success("--depth <DEPTH>\n"));
            }
            if program == "jj" && words.starts_with(&["workspace", "add"]) {
                state.workspace_adds += 1;
                std::fs::create_dir_all(args.last().unwrap())?;
                return Ok(success(""));
            }
            if program == "jj" && args.last().is_some_and(|v| v == JJ_REVISION_TEMPLATE) {
                let (change, commit, conflict, empty) = state
                    .revisions
                    .pop_front()
                    .unwrap_or_else(|| ("change-a".into(), "a".repeat(40), false, false));
                return Ok(success(&format!(
                    "{change}\n{commit}\n{conflict}\n{empty}\n"
                )));
            }
            if program == "jj" && args.last().is_some_and(|v| v == JJ_OPERATION_TEMPLATE) {
                if words.contains(&"--at-op=@") {
                    let recent = state
                        .seen_operations
                        .iter()
                        .rev()
                        .take(2)
                        .cloned()
                        .collect::<Vec<_>>();
                    return Ok(success(&format!("{}\n", recent.join("\n"))));
                }
                let operation = state
                    .operations
                    .pop_front()
                    .unwrap_or_else(|| "op-a".into());
                state.seen_operations.push(operation.clone());
                return Ok(success(&format!("{operation}\n")));
            }
            if program == "jj" && words.contains(&"status") {
                state.status_calls += 1;
                return Ok(success(""));
            }
            Ok(success(""))
        }
    }

    #[derive(Clone, Default)]
    struct RecordingSink(Arc<Mutex<Vec<VcsSnapshotObserved>>>);

    impl VcsObservationSink for RecordingSink {
        fn record(
            &self,
            observation: &VcsSnapshotObserved,
            _base_revision: Option<u64>,
        ) -> VcsResult<()> {
            self.0.lock().unwrap().push(observation.clone());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FailOnceSink {
        failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        recorded: RecordingSink,
    }

    impl VcsObservationSink for FailOnceSink {
        fn record(
            &self,
            observation: &VcsSnapshotObserved,
            base_revision: Option<u64>,
        ) -> VcsResult<()> {
            if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(VcsError::new(VcsErrorCode::Io, "injected journal failure"));
            }
            self.recorded.record(observation, base_revision)
        }
    }

    struct Fixture {
        _temp: TempDir,
        repository: PathBuf,
        managed: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let repository = temp.path().join("repo");
            let managed = temp.path().join("managed");
            std::fs::create_dir_all(repository.join(".jj")).unwrap();
            std::fs::create_dir_all(&managed).unwrap();
            Self {
                _temp: temp,
                repository,
                managed,
            }
        }
    }

    fn manager(
        fixture: &Fixture,
        runner: MockRunner,
        sink: RecordingSink,
    ) -> VcsManager<MockRunner, RecordingSink> {
        VcsManager::with_components(&fixture.repository, &fixture.managed, runner, sink).unwrap()
    }

    fn request(workspace_id: &str, base: char) -> WorkspaceEnsureRequest {
        WorkspaceEnsureRequest {
            workspace_id: workspace_id.into(),
            task_id: format!("task-for-{workspace_id}"),
            backend: VcsBackendKind::Jujutsu,
            base_revision: base.to_string().repeat(40),
        }
    }

    fn success(stdout: &str) -> CommandOutput {
        CommandOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            success: true,
        }
    }

    #[test]
    fn delegated_receipt_imports_once_without_running_jj_and_replays_observation() {
        let fixture = Fixture::new();
        let runner = MockRunner::new();
        let sink = RecordingSink::default();
        let manager = manager(&fixture, runner.clone(), sink.clone());
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();
        let operation = Uuid::new_v4();
        let report = DelegatedSnapshotReport {
            operation_id: operation,
            task_id: "task-for-task-a".into(),
            workspace_id: "task-a".into(),
            execution_id: Some("exec-a".into()),
            before: JujutsuState {
                logical_change_id: "changea".into(),
                materialized_revision: "a".repeat(40),
                vcs_operation_id: "opa".into(),
                conflicted: false,
                empty: false,
            },
            after: JujutsuState {
                logical_change_id: "changea".into(),
                materialized_revision: "b".repeat(40),
                vcs_operation_id: "opb".into(),
                conflicted: false,
                empty: false,
            },
        };
        let before_calls = runner.state.lock().unwrap().status_calls;
        assert!(
            manager
                .reserve_delegated_snapshot("task-a", operation, Some("exec-a"))
                .unwrap()
        );
        assert!(
            !manager
                .reserve_delegated_snapshot("task-a", operation, Some("exec-a"))
                .unwrap()
        );
        assert!(
            manager
                .workspace_has_unreconciled_snapshots("task-a")
                .unwrap()
        );
        assert!(
            manager
                .delegated_snapshot_result("task-a", operation, Some("exec-a"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            manager
                .import_delegated_snapshot("task-a", operation, Some("exec-b"), &report)
                .unwrap_err()
                .code,
            VcsErrorCode::OperationConflict
        );
        assert_eq!(
            manager
                .import_delegated_snapshot("task-a", operation, Some("exec-a"), &report)
                .unwrap()
                .after
                .materialized_revision,
            "b".repeat(40)
        );
        assert!(
            manager
                .import_delegated_snapshot("task-a", operation, Some("exec-a"), &report)
                .unwrap()
                .replayed
        );
        assert_eq!(runner.state.lock().unwrap().status_calls, before_calls);
        assert_eq!(sink.0.lock().unwrap().len(), 1);
        assert!(
            !manager
                .workspace_has_unreconciled_snapshots("task-a")
                .unwrap()
        );
    }

    #[test]
    fn delegated_observation_fault_keeps_workspace_fenced_until_exact_replay() {
        let fixture = Fixture::new();
        let runner = MockRunner::new();
        let sink = FailOnceSink::default();
        let manager = VcsManager::with_observation_sink(
            &fixture.repository,
            &fixture.managed,
            runner.clone(),
            sink.clone(),
        )
        .unwrap();
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();
        let operation = Uuid::new_v4();
        let before_calls = runner.state.lock().unwrap().status_calls;
        assert!(
            manager
                .reserve_delegated_snapshot("task-a", operation, Some("exec-a"))
                .unwrap()
        );
        let report = DelegatedSnapshotReport {
            operation_id: operation,
            task_id: "task-for-task-a".into(),
            workspace_id: "task-a".into(),
            execution_id: Some("exec-a".into()),
            before: JujutsuState {
                logical_change_id: "changea".into(),
                materialized_revision: "a".repeat(40),
                vcs_operation_id: "opa".into(),
                conflicted: false,
                empty: false,
            },
            after: JujutsuState {
                logical_change_id: "changea".into(),
                materialized_revision: "b".repeat(40),
                vcs_operation_id: "opb".into(),
                conflicted: false,
                empty: false,
            },
        };
        assert_eq!(
            manager
                .import_delegated_snapshot("task-a", operation, Some("exec-a"), &report)
                .unwrap_err()
                .code,
            VcsErrorCode::Io
        );
        assert!(
            manager
                .workspace_has_unreconciled_snapshots("task-a")
                .unwrap()
        );
        assert_eq!(
            manager
                .reserve_delegated_snapshot("task-a", Uuid::new_v4(), Some("exec-a"))
                .unwrap_err()
                .code,
            VcsErrorCode::ReconciliationRequired
        );
        assert!(
            manager
                .delegated_snapshot_result("task-a", operation, Some("exec-a"))
                .unwrap()
                .unwrap()
                .replayed
        );
        assert!(
            !manager
                .workspace_has_unreconciled_snapshots("task-a")
                .unwrap()
        );
        assert_eq!(sink.recorded.0.lock().unwrap().len(), 1);
        assert_eq!(runner.state.lock().unwrap().status_calls, before_calls);
    }

    #[test]
    fn unavailable_jj_fails_without_git_fallback() {
        let fixture = Fixture::new();
        let runner = MockRunner::new();
        runner.state.lock().unwrap().jj_available = false;
        let manager = manager(&fixture, runner, RecordingSink::default());
        assert_eq!(
            manager.capabilities().jj.state,
            CapabilityState::Unsupported
        );
        assert_eq!(
            manager
                .workspace_ensure(&request("task-a", 'a'))
                .unwrap_err()
                .code,
            VcsErrorCode::CapabilityUnsupported
        );
    }

    #[test]
    fn ensure_is_idempotent_and_conflicts_on_changed_request() {
        let fixture = Fixture::new();
        let runner = MockRunner::new();
        let manager = manager(&fixture, runner.clone(), RecordingSink::default());
        assert!(
            manager
                .workspace_ensure(&request("task-a", 'a'))
                .unwrap()
                .created
        );
        assert!(
            !manager
                .workspace_ensure(&request("task-a", 'a'))
                .unwrap()
                .created
        );
        assert_eq!(runner.state.lock().unwrap().workspace_adds, 1);
        assert_eq!(
            manager
                .workspace_ensure(&request("task-a", 'b'))
                .unwrap_err()
                .code,
            VcsErrorCode::WorkspaceConflict
        );
        let mut changed_task = request("task-a", 'a');
        changed_task.task_id = "other-task".into();
        assert_eq!(
            manager.workspace_ensure(&changed_task).unwrap_err().code,
            VcsErrorCode::WorkspaceConflict
        );
    }

    #[test]
    fn rejects_git_backend_and_arbitrary_revsets() {
        let fixture = Fixture::new();
        let manager = manager(&fixture, MockRunner::new(), RecordingSink::default());
        let mut request = request("task-a", 'a');
        request.backend = VcsBackendKind::Git;
        assert_eq!(
            manager.workspace_ensure(&request).unwrap_err().code,
            VcsErrorCode::BackendUnsupported
        );
        request = WorkspaceEnsureRequest {
            workspace_id: "task-b".into(),
            task_id: "logical-task-b".into(),
            backend: VcsBackendKind::Jujutsu,
            base_revision: "origin/main".into(),
        };
        assert_eq!(
            manager.workspace_ensure(&request).unwrap_err().code,
            VcsErrorCode::InvalidRequest
        );
    }

    #[test]
    fn snapshot_tracks_logical_change_and_replays_completed_receipt() {
        let fixture = Fixture::new();
        let initial = "a".repeat(40);
        let updated = "b".repeat(40);
        let runner = MockRunner::new().with_revisions(
            [
                ("change-a".into(), initial.clone(), false, false),
                ("change-a".into(), initial.clone(), false, false),
                ("change-a".into(), updated.clone(), false, false),
            ],
            ["op-a".into(), "op-a".into(), "op-b".into()],
        );
        let sink = RecordingSink::default();
        let manager = manager(&fixture, runner.clone(), sink.clone());
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();

        let operation_id = Uuid::new_v4();
        let result = manager
            .snapshot("task-a", operation_id, Some("exec-a"))
            .unwrap();
        assert_eq!(result.before.logical_change_id, "change-a");
        assert_eq!(result.after.logical_change_id, "change-a");
        assert_eq!(result.before.materialized_revision, initial);
        assert_eq!(result.after.materialized_revision, updated);
        assert_eq!(result.observation.vcs_operation_id, "op-b");
        assert_eq!(
            result.observation.task_id.as_deref(),
            Some("task-for-task-a")
        );
        assert_eq!(result.observation.execution_id.as_deref(), Some("exec-a"));
        assert_eq!(result.observation.operation_id, operation_id);
        assert!(!result.replayed);
        assert!(!result.reconciled);

        let replay = manager
            .snapshot("task-a", operation_id, Some("exec-a"))
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(
            manager
                .snapshot("task-a", operation_id, Some("exec-b"))
                .unwrap_err()
                .code,
            VcsErrorCode::OperationConflict
        );
        assert_eq!(runner.state.lock().unwrap().status_calls, 1);
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn completed_snapshot_replays_pending_observation_without_second_status() {
        let fixture = Fixture::new();
        let runner = MockRunner::new();
        let sink = FailOnceSink::default();
        let manager = VcsManager::with_observation_sink(
            &fixture.repository,
            &fixture.managed,
            runner.clone(),
            sink.clone(),
        )
        .unwrap();
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();
        let operation = Uuid::new_v4();
        assert_eq!(
            manager
                .snapshot("task-a", operation, Some("exec-a"))
                .unwrap_err()
                .code,
            VcsErrorCode::Io
        );
        let receipt: SnapshotReceipt =
            read_json_record(&manager.operation_receipt_path(operation)).unwrap();
        assert_eq!(receipt.state, ReceiptState::Completed);
        assert!(receipt.observation_pending);
        assert!(
            manager
                .workspace_has_unreconciled_snapshots("task-a")
                .unwrap()
        );
        assert_eq!(runner.state.lock().unwrap().status_calls, 1);
        assert!(
            manager
                .snapshot("task-a", operation, Some("exec-a"))
                .unwrap()
                .replayed
        );
        assert_eq!(runner.state.lock().unwrap().status_calls, 1);
        assert_eq!(sink.recorded.0.lock().unwrap().len(), 1);
        let receipt: SnapshotReceipt =
            read_json_record(&manager.operation_receipt_path(operation)).unwrap();
        assert!(!receipt.observation_pending);
        assert!(
            !manager
                .workspace_has_unreconciled_snapshots("task-a")
                .unwrap()
        );
    }

    #[test]
    fn accepted_receipt_requires_reconciliation_instead_of_replay() {
        let fixture = Fixture::new();
        let runner = MockRunner::new();
        let manager = manager(&fixture, runner.clone(), RecordingSink::default());
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();
        let operation_id = Uuid::new_v4();
        write_json_atomic(
            &manager.operation_receipt_path(operation_id),
            &SnapshotReceipt {
                schema_version: SCHEMA_VERSION,
                operation_id,
                request_fingerprint: snapshot_fingerprint("task-a", Some("exec-a")),
                state: ReceiptState::Accepted,
                task_id: Some("task-for-task-a".into()),
                execution_id: Some("exec-a".into()),
                before: Some(manager.inspect("task-a").unwrap().jj),
                result: None,
                observation_pending: false,
                observation_base_revision: None,
                delegated: false,
            },
        )
        .unwrap();

        assert_eq!(
            manager
                .snapshot("task-a", operation_id, Some("exec-a"))
                .unwrap_err()
                .code,
            VcsErrorCode::ReconciliationRequired
        );
        assert_eq!(runner.state.lock().unwrap().status_calls, 0);
    }

    #[test]
    fn reconcile_backfills_durable_transition_without_replaying_status() {
        let fixture = Fixture::new();
        let initial = "a".repeat(40);
        let updated = "b".repeat(40);
        let runner = MockRunner::new().with_revisions(
            [
                ("change-a".into(), initial.clone(), false, false),
                ("change-a".into(), updated.clone(), false, false),
            ],
            ["op-a".into(), "op-b".into()],
        );
        let sink = RecordingSink::default();
        let manager = manager(&fixture, runner.clone(), sink.clone());
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();

        let operation_id = Uuid::new_v4();
        write_json_atomic(
            &manager.operation_receipt_path(operation_id),
            &SnapshotReceipt {
                schema_version: SCHEMA_VERSION,
                operation_id,
                request_fingerprint: snapshot_fingerprint("task-a", Some("exec-a")),
                state: ReceiptState::Accepted,
                task_id: Some("task-for-task-a".into()),
                execution_id: Some("exec-a".into()),
                before: Some(JujutsuState {
                    logical_change_id: "change-a".into(),
                    materialized_revision: initial.clone(),
                    vcs_operation_id: "op-a".into(),
                    conflicted: false,
                    empty: false,
                }),
                result: None,
                observation_pending: false,
                observation_base_revision: None,
                delegated: false,
            },
        )
        .unwrap();

        let result = manager.reconcile_snapshot("task-a", operation_id).unwrap();
        assert!(result.reconciled);
        assert!(!result.replayed);
        assert_eq!(result.before.materialized_revision, initial);
        assert_eq!(result.after.materialized_revision, updated);
        assert_eq!(runner.state.lock().unwrap().status_calls, 0);
        assert_eq!(sink.0.lock().unwrap().len(), 1);

        let replay = manager.reconcile_snapshot("task-a", operation_id).unwrap();
        assert!(replay.replayed);
        assert!(replay.reconciled);
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn reconcile_refuses_intervening_jj_operation() {
        let fixture = Fixture::new();
        let runner = MockRunner::new().with_revisions(
            [
                ("change-a".into(), "a".repeat(40), false, false),
                ("change-a".into(), "b".repeat(40), false, false),
                ("change-a".into(), "c".repeat(40), false, false),
            ],
            ["op-a".into(), "op-b".into(), "op-c".into()],
        );
        let manager = manager(&fixture, runner.clone(), RecordingSink::default());
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();
        let operation_id = Uuid::new_v4();
        write_json_atomic(
            &manager.operation_receipt_path(operation_id),
            &SnapshotReceipt {
                schema_version: SCHEMA_VERSION,
                operation_id,
                request_fingerprint: snapshot_fingerprint("task-a", Some("exec-a")),
                state: ReceiptState::Accepted,
                task_id: Some("task-for-task-a".into()),
                execution_id: Some("exec-a".into()),
                before: Some(JujutsuState {
                    logical_change_id: "change-a".into(),
                    materialized_revision: "a".repeat(40),
                    vcs_operation_id: "op-a".into(),
                    conflicted: false,
                    empty: false,
                }),
                result: None,
                observation_pending: false,
                observation_base_revision: None,
                delegated: false,
            },
        )
        .unwrap();
        manager.inspect("task-a").unwrap(); // unrelated later operation
        assert_eq!(
            manager
                .reconcile_snapshot("task-a", operation_id)
                .unwrap_err()
                .code,
            VcsErrorCode::ReconciliationRequired
        );
        assert_eq!(runner.state.lock().unwrap().status_calls, 0);
    }

    #[test]
    fn reconcile_keeps_accepted_receipt_when_no_durable_transition_exists() {
        let fixture = Fixture::new();
        let revision = "a".repeat(40);
        let runner = MockRunner::new().with_revisions(
            [("change-a".into(), revision.clone(), false, false)],
            ["op-a".into()],
        );
        let manager = manager(&fixture, runner.clone(), RecordingSink::default());
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();

        let operation_id = Uuid::new_v4();
        write_json_atomic(
            &manager.operation_receipt_path(operation_id),
            &SnapshotReceipt {
                schema_version: SCHEMA_VERSION,
                operation_id,
                request_fingerprint: snapshot_fingerprint("task-a", Some("exec-a")),
                state: ReceiptState::Accepted,
                task_id: Some("task-for-task-a".into()),
                execution_id: Some("exec-a".into()),
                before: Some(JujutsuState {
                    logical_change_id: "change-a".into(),
                    materialized_revision: revision,
                    vcs_operation_id: "op-a".into(),
                    conflicted: false,
                    empty: false,
                }),
                result: None,
                observation_pending: false,
                observation_base_revision: None,
                delegated: false,
            },
        )
        .unwrap();

        let error = manager
            .reconcile_snapshot("task-a", operation_id)
            .unwrap_err();
        assert_eq!(error.code, VcsErrorCode::ReconciliationRequired);
        assert_eq!(runner.state.lock().unwrap().status_calls, 0);

        let receipt: SnapshotReceipt =
            read_json_record(&manager.operation_receipt_path(operation_id)).unwrap();
        assert_eq!(receipt.state, ReceiptState::Accepted);
        assert!(receipt.result.is_none());
    }

    #[test]
    fn reconcile_rejects_legacy_accepted_receipt_without_before_state() {
        let fixture = Fixture::new();
        let runner = MockRunner::new();
        let manager = manager(&fixture, runner, RecordingSink::default());
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();

        let operation_id = Uuid::new_v4();
        write_json_atomic(
            &manager.operation_receipt_path(operation_id),
            &SnapshotReceipt {
                schema_version: SCHEMA_VERSION,
                operation_id,
                request_fingerprint: legacy_snapshot_fingerprint("task-a"),
                state: ReceiptState::Accepted,
                task_id: None,
                execution_id: None,
                before: None,
                result: None,
                observation_pending: false,
                observation_base_revision: None,
                delegated: false,
            },
        )
        .unwrap();

        let error = manager
            .reconcile_snapshot("task-a", operation_id)
            .unwrap_err();
        assert_eq!(error.code, VcsErrorCode::ReconciliationRequired);
    }

    #[test]
    fn legacy_completed_receipt_replays_only_without_execution_rebinding() {
        let fixture = Fixture::new();
        let manager = manager(&fixture, MockRunner::new(), RecordingSink::default());
        manager.workspace_ensure(&request("task-a", 'a')).unwrap();

        let operation_id = Uuid::new_v4();
        let state = manager.inspect("task-a").unwrap().jj;
        let observation = VcsSnapshotObserved {
            operation_id,
            workspace_id: "task-a".into(),
            task_id: None,
            execution_id: None,
            backend: VcsBackendKind::Jujutsu,
            logical_change_id: state.logical_change_id.clone(),
            before_revision: state.materialized_revision.clone(),
            after_revision: state.materialized_revision.clone(),
            vcs_operation_id: state.vcs_operation_id.clone(),
            conflicted: state.conflicted,
            empty: state.empty,
        };
        let result = SnapshotResult {
            operation_id,
            replayed: false,
            reconciled: false,
            before: state.clone(),
            after: state,
            observation,
        };
        write_json_atomic(
            &manager.operation_receipt_path(operation_id),
            &SnapshotReceipt {
                schema_version: SCHEMA_VERSION,
                operation_id,
                request_fingerprint: legacy_snapshot_fingerprint("task-a"),
                state: ReceiptState::Completed,
                task_id: None,
                execution_id: None,
                before: None,
                result: Some(result),
                observation_pending: false,
                observation_base_revision: None,
                delegated: false,
            },
        )
        .unwrap();

        assert!(
            manager
                .snapshot("task-a", operation_id, None)
                .unwrap()
                .replayed
        );
        assert_eq!(
            manager
                .snapshot("task-a", operation_id, Some("exec-a"))
                .unwrap_err()
                .code,
            VcsErrorCode::OperationConflict
        );
    }

    #[test]
    fn sibling_workspaces_are_distinct_and_need_no_git_checkout() {
        let fixture = Fixture::new();
        let manager = manager(&fixture, MockRunner::new(), RecordingSink::default());
        let first = manager.workspace_ensure(&request("task-a", 'a')).unwrap();
        let second = manager.workspace_ensure(&request("task-b", 'a')).unwrap();
        assert_ne!(first.workspace.path, second.workspace.path);
        let managed = std::fs::canonicalize(&fixture.managed).unwrap();
        assert!(first.workspace.path.starts_with(&managed));
        assert!(second.workspace.path.starts_with(&managed));
        assert!(!fixture.repository.join(".git").exists());
        let inspected = manager.inspect("task-a").unwrap();
        assert_eq!(inspected.workspace_id, "task-a");
        assert_eq!(inspected.task_id.as_deref(), Some("task-for-task-a"));
    }
}
