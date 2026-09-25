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
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<SnapshotResult>,
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
struct CommandOutput {
    stdout: String,
    stderr: String,
    success: bool,
}

trait CommandRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> std::io::Result<CommandOutput>;
}

#[derive(Clone, Copy, Debug, Default)]
struct SystemCommandRunner;

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
    fn record(&self, observation: &VcsSnapshotObserved);
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoopObservationSink;

impl VcsObservationSink for NoopObservationSink {
    fn record(&self, _observation: &VcsSnapshotObserved) {}
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

impl<R: CommandRunner, S: VcsObservationSink> VcsManager<R, S> {
    fn with_components(
        repository_root: &Path,
        managed_root: &Path,
        runner: R,
        observation_sink: S,
    ) -> VcsResult<Self> {
        let repository_root =
            canonical_existing_directory(repository_root, "repository root")?;
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
                    Capability::supported(
                        None,
                        Some("jj git clone advertises --depth".to_owned()),
                    )
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
        let git_lfs = match self
            .runner
            .run("git-lfs", &strings(&["--version"]), Some(&self.repository_root))
        {
            Ok(output) if output.success => Capability::supported(
                first_nonempty_line(&output.stdout).map(str::to_owned),
                Some("git-lfs executable is available; repository usage is not inferred".to_owned()),
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
        self.run_checked("jj", &args, Some(&self.repository_root), "create jj workspace")?;

        let canonical_path = canonical_existing_directory(&workspace_path, "jj workspace")?;
        ensure_descendant(&self.managed_root, &canonical_path, "jj workspace")?;
        let record = WorkspaceRecord {
            schema_version: SCHEMA_VERSION,
            request_fingerprint: fingerprint,
            repository_root: self.repository_root.clone(),
            workspace_id: request.workspace_id.clone(),
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

    pub(crate) fn snapshot(
        &self,
        workspace_id: &str,
        operation_id: Uuid,
    ) -> VcsResult<SnapshotResult> {
        validate_workspace_id(workspace_id)?;
        let request_fingerprint = snapshot_fingerprint(workspace_id);
        let receipt_path = self.operation_receipt_path(operation_id);
        let _lock = self.acquire_store_lock()?;

        if receipt_path.exists() {
            let receipt: SnapshotReceipt = read_json_record(&receipt_path)?;
            validate_receipt(&receipt, operation_id)?;
            if receipt.request_fingerprint != request_fingerprint {
                return Err(VcsError::new(
                    VcsErrorCode::OperationConflict,
                    "operation_id was already accepted for a different VCS snapshot request",
                ));
            }
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
            result: None,
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
            workspace_id: workspace_id.to_owned(),
            task_id: None,
            execution_id: None,
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
            before,
            after,
            observation: observation.clone(),
        };
        let completed = SnapshotReceipt {
            schema_version: SCHEMA_VERSION,
            operation_id,
            request_fingerprint,
            state: ReceiptState::Completed,
            result: Some(result.clone()),
        };
        write_json_atomic(&receipt_path, &completed)?;
        self.observation_sink.record(&observation);
        Ok(result)
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
        let fields = revision
            .stdout
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>();
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
        Ok(output) if output.success => Capability::supported(
            first_nonempty_line(&output.stdout).map(str::to_owned),
            None,
        ),
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

fn validate_exact_revision(revision: &str) -> VcsResult<()> {
    if matches!(revision.len(), 40 | 64)
        && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(VcsError::new(
            VcsErrorCode::InvalidRequest,
            "base_revision must be an exact 40- or 64-character hexadecimal revision id",
        ))
    }
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
    hasher.update(request.base_revision.as_bytes());
    hex_digest(&hasher.finalize())
}

fn snapshot_fingerprint(workspace_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"temote-vcs-snapshot-v1\0");
    hasher.update(workspace_id.as_bytes());
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
            format!("create temporary VCS record {}: {error}", temporary.display()),
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
    let mut file = options.open(path).map_err(|error| {
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
                let operation = state
                    .operations
                    .pop_front()
                    .unwrap_or_else(|| "op-a".into());
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
        fn record(&self, observation: &VcsSnapshotObserved) {
            self.0.lock().unwrap().push(observation.clone());
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
            manager.workspace_ensure(&request("task-a", 'a')).unwrap_err().code,
            VcsErrorCode::CapabilityUnsupported
        );
    }

    #[test]
    fn ensure_is_idempotent_and_conflicts_on_changed_request() {
        let fixture = Fixture::new();
        let runner = MockRunner::new();
        let manager = manager(&fixture, runner.clone(), RecordingSink::default());
        assert!(manager.workspace_ensure(&request("task-a", 'a')).unwrap().created);
        assert!(!manager.workspace_ensure(&request("task-a", 'a')).unwrap().created);
        assert_eq!(runner.state.lock().unwrap().workspace_adds, 1);
        assert_eq!(
            manager.workspace_ensure(&request("task-a", 'b')).unwrap_err().code,
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
        let result = manager.snapshot("task-a", operation_id).unwrap();
        assert_eq!(result.before.logical_change_id, "change-a");
        assert_eq!(result.after.logical_change_id, "change-a");
        assert_eq!(result.before.materialized_revision, initial);
        assert_eq!(result.after.materialized_revision, updated);
        assert_eq!(result.observation.vcs_operation_id, "op-b");
        assert!(!result.replayed);

        let replay = manager.snapshot("task-a", operation_id).unwrap();
        assert!(replay.replayed);
        assert_eq!(runner.state.lock().unwrap().status_calls, 1);
        assert_eq!(sink.0.lock().unwrap().len(), 1);
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
                request_fingerprint: snapshot_fingerprint("task-a"),
                state: ReceiptState::Accepted,
                result: None,
            },
        )
        .unwrap();

        assert_eq!(
            manager.snapshot("task-a", operation_id).unwrap_err().code,
            VcsErrorCode::ReconciliationRequired
        );
        assert_eq!(runner.state.lock().unwrap().status_calls, 0);
    }

    #[test]
    fn sibling_workspaces_are_distinct_and_need_no_git_checkout() {
        let fixture = Fixture::new();
        let manager = manager(&fixture, MockRunner::new(), RecordingSink::default());
        let first = manager.workspace_ensure(&request("task-a", 'a')).unwrap();
        let second = manager.workspace_ensure(&request("task-b", 'a')).unwrap();
        assert_ne!(first.workspace.path, second.workspace.path);
        assert!(first.workspace.path.starts_with(&fixture.managed));
        assert!(second.workspace.path.starts_with(&fixture.managed));
        assert!(!fixture.repository.join(".git").exists());
        assert_eq!(manager.inspect("task-a").unwrap().workspace_id, "task-a");
    }
}
