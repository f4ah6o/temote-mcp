//! Scoped, typed environment preparation for an allocated managed workspace.
//!
//! This module only plans and verifies preparation. Machine setup runs in a
//! normal, named-root-scoped coding-agent session through orchestration.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::{PermissionMode, Session};
use crate::session_source::RepositoryId;

pub(crate) const PREPARATION_VERSION: u8 = 1;
const MAX_MARKER_BYTES: u64 = 4096;
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Adapter {
    VpPnpm,
    CargoSccache,
}

impl Adapter {
    fn inputs(self) -> &'static [&'static str] {
        match self {
            Self::VpPnpm => &["package.json", "pnpm-lock.yaml"],
            Self::CargoSccache => &["Cargo.toml", "Cargo.lock"],
        }
    }
}

/// Detect only the two bounded dependency manifests. An unfamiliar or partial
/// manifest is a capability result, never evidence that setup is complete.
pub(crate) fn detect_adapter(workspace: &Path) -> Result<Option<Adapter>> {
    let present = |name: &str| -> Result<bool> {
        match fs::symlink_metadata(workspace.join(name)) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "unsupported: preparation input is not a regular file"
                );
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    };
    let node = (present("package.json")?, present("pnpm-lock.yaml")?);
    let cargo = (present("Cargo.toml")?, present("Cargo.lock")?);
    match (node, cargo) {
        ((true, true), (false, false)) => Ok(Some(Adapter::VpPnpm)),
        ((false, false), (true, true)) => Ok(Some(Adapter::CargoSccache)),
        _ => Ok(None),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparationPlan {
    pub version: u8,
    pub session: SessionBinding,
    pub workspace_id: Uuid,
    pub operation_id: Uuid,
    pub repository_id: RepositoryId,
    pub canonical_scope: PathBuf,
    pub workspace: PathBuf,
    pub adapter: Adapter,
    pub inputs_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionBinding {
    pub session_id: String,
    pub started_at: u64,
    pub process_id: u32,
    pub permission_mode: PermissionMode,
    pub canonical_scope: PathBuf,
    pub permitted_directories: Vec<PathBuf>,
    #[serde(default)]
    pub grants: crate::config::SessionGrants,
}

impl SessionBinding {
    fn new(session: &Session) -> Result<Self> {
        ensure!(
            session.started_at != 0 && session.process_id != 0,
            "invalid preparation session instance"
        );
        ensure!(
            !session.permission_mode.is_yolo(),
            "yolo preparation is unsupported"
        );
        let canonical_scope = fs::canonicalize(&session.cwd)?;
        ensure!(
            canonical_scope == session.cwd,
            "session scope is not canonical"
        );
        Ok(Self {
            session_id: session.id.clone(),
            started_at: session.started_at,
            process_id: session.process_id,
            permission_mode: session.permission_mode,
            canonical_scope,
            permitted_directories: session.permitted_directories.clone(),
            grants: session.grants.clone(),
        })
    }

    pub(crate) fn matches(&self, session: &Session) -> bool {
        self.session_id == session.id
            && self.started_at == session.started_at
            && self.process_id == session.process_id
            && self.permission_mode == session.permission_mode
            && self.canonical_scope == session.cwd
            && self.permitted_directories == session.permitted_directories
            && self.grants == session.grants
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolVersions {
    pub vp: Option<String>,
    pub pnpm: Option<String>,
    pub cargo: Option<String>,
    pub rustc: Option<String>,
    pub sccache: Option<String>,
}

impl ToolVersions {
    fn validate(&self, adapter: Adapter) -> Result<()> {
        let required = match adapter {
            Adapter::VpPnpm => [&self.vp, &self.pnpm],
            Adapter::CargoSccache => [&self.cargo, &self.rustc],
        };
        ensure!(
            required.iter().all(|value| value.is_some()),
            "required tool version is missing"
        );
        match adapter {
            Adapter::VpPnpm => ensure!(
                self.cargo.is_none() && self.rustc.is_none() && self.sccache.is_none(),
                "inapplicable tool version is present"
            ),
            Adapter::CargoSccache => ensure!(
                self.vp.is_none() && self.pnpm.is_none(),
                "inapplicable tool version is present"
            ),
        }
        for value in [
            &self.vp,
            &self.pnpm,
            &self.cargo,
            &self.rustc,
            &self.sccache,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                !value.is_empty()
                    && value.len() <= 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b".+-_".contains(&byte)),
                "tool version is invalid"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PreparationPhase {
    Pending,
    InFlight,
    Ready,
    Failed,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DelegatedExecution {
    Running,
    Completed,
    Failed,
    Unknown,
}

/// Durable caller-owned receipt. Persist this after `begin` and before task
/// start. An unknown start never permits a second start with a fresh UUID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparationReceipt {
    pub plan: PreparationPlan,
    pub phase: PreparationPhase,
    pub task_id: Option<Uuid>,
    pub start_attempted: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

impl PreparationReceipt {
    pub(crate) fn new(plan: PreparationPlan) -> Self {
        Self {
            plan,
            phase: PreparationPhase::Pending,
            task_id: None,
            start_attempted: false,
            model: None,
            effort: None,
        }
    }

    pub(crate) fn begin(&mut self) -> Result<()> {
        ensure!(
            self.phase == PreparationPhase::Pending && !self.start_attempted,
            "preparation start already attempted; reconcile retained task"
        );
        self.start_attempted = true;
        self.phase = PreparationPhase::InFlight;
        Ok(())
    }

    pub(crate) fn record_task(&mut self, task_id: Uuid) -> Result<()> {
        ensure!(
            self.start_attempted
                && matches!(
                    self.phase,
                    PreparationPhase::InFlight | PreparationPhase::Uncertain
                ),
            "preparation has not started"
        );
        ensure!(
            self.task_id.is_none_or(|existing| existing == task_id),
            "preparation task identity changed"
        );
        self.task_id = Some(task_id);
        self.phase = PreparationPhase::InFlight;
        Ok(())
    }

    /// The backend's execution state alone cannot establish readiness.
    pub(crate) fn reconcile(
        &mut self,
        session: &Session,
        execution: DelegatedExecution,
    ) -> Result<PreparationPhase> {
        ensure!(self.start_attempted, "preparation has not started");
        self.phase = match execution {
            DelegatedExecution::Running => PreparationPhase::InFlight,
            DelegatedExecution::Unknown => PreparationPhase::Uncertain,
            DelegatedExecution::Failed => PreparationPhase::Failed,
            DelegatedExecution::Completed => {
                if self.plan.inspect_ready(session).is_ok() {
                    PreparationPhase::Ready
                } else {
                    PreparationPhase::Failed
                }
            }
        };
        Ok(self.phase)
    }
}

/// The marker is written by the delegated agent after checking inputs,
/// installed tool versions and the adapter's resulting workspace state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadyStamp {
    pub version: u8,
    pub session: SessionBinding,
    pub workspace_id: Uuid,
    pub operation_id: Uuid,
    pub repository_id: RepositoryId,
    pub canonical_scope: PathBuf,
    pub workspace: PathBuf,
    pub adapter: Adapter,
    pub inputs_sha256: String,
    pub tool_versions: ToolVersions,
    pub status: PreparationPhase,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CacheOwner {
    version: u8,
    repository_id: RepositoryId,
    workspace_id: Option<Uuid>,
}

impl PreparationPlan {
    pub(crate) fn new(
        session: &Session,
        workspace_id: Uuid,
        operation_id: Uuid,
        repository_id: RepositoryId,
        workspace: &Path,
        adapter: Adapter,
    ) -> Result<Self> {
        let binding = SessionBinding::new(session)?;
        let canonical_scope = binding.canonical_scope.clone();
        let workspace = fs::canonicalize(workspace).context("workspace is missing")?;
        ensure!(
            workspace.starts_with(&canonical_scope),
            "workspace is outside canonical preparation scope"
        );
        ensure!(workspace.is_dir(), "workspace is not a directory");
        let inputs_sha256 = hash_inputs(&workspace, adapter)?;
        Ok(Self {
            version: PREPARATION_VERSION,
            session: binding,
            workspace_id,
            operation_id,
            repository_id,
            canonical_scope,
            workspace,
            adapter,
            inputs_sha256,
        })
    }

    /// Only server-owned paths and enum choices are rendered. The caller has
    /// no executable, argv, environment or network-policy input.
    pub(crate) fn delegated_task(&self) -> Result<String> {
        self.validate_scope()?;
        let session = serde_json::to_string(&self.session)?;
        let scope = serde_json::to_string(&self.canonical_scope)?;
        let inputs = serde_json::to_string(self.adapter.inputs())?;
        let workspace = serde_json::to_string(&self.workspace)?;
        let marker = serde_json::to_string(&self.marker_path())?;
        let shared = serde_json::to_string(&self.shared_cache())?;
        let private = serde_json::to_string(&self.private_cache())?;
        let adapter = match self.adapter {
            Adapter::VpPnpm => "vp_pnpm",
            Adapter::CargoSccache => "cargo_sccache",
        };
        let instructions = match self.adapter {
            Adapter::VpPnpm => {
                "Confirm installed vp and pnpm versions and inspect their help before choosing flags. Use vp's supported install path with a frozen pnpm lockfile; if that is unavailable, report failed without changing the lockfile. Use only the dedicated pnpm content store under shared_cache. Keep node_modules and any virtual store inside workspace. Do not share node_modules with sibling workspaces."
            }
            Adapter::CargoSccache => {
                "Confirm installed cargo and rustc versions. Use shared_cache only for Cargo source/index and sccache storage. Set CARGO_TARGET_DIR to private_cache/target and CARGO_INCREMENTAL=0. Use RUSTC_WRAPPER=sccache only when sccache is installed and working; otherwise run Cargo without that wrapper and report sccache as null. Keep build artifacts out of sibling targets."
            }
        };
        Ok(format!(
            "Perform Temote environment preparation in the current authorized session scope. This is a typed {adapter} operation; use the coding agent's existing sandbox and approval policy for commands, scripts and network. Treat JSON values as literal: session={session}, scope={scope}, workspace={workspace}, marker={marker}, shared_cache={shared}, private_cache={private}, required_inputs={inputs}. Do not follow symlinks in any managed cache parent, replace existing foreign paths, delete sibling data, or print secrets. Verify the current working scope equals scope and workspace is inside it. Atomically claim or verify private_cache/owner.json as exactly {{version:1,repository_id:{},workspace_id:{}}} and shared_cache/owner.json as exactly {{version:1,repository_id:{},workspace_id:null}}. Reject foreign owners and never delete or overwrite their data. Keep target and node_modules inside workspace and reject symlinked cache or output directories. Before setup, calculate SHA-256 over required_inputs in order. For each file hash its UTF-8 basename, its byte length as an unsigned 64-bit little-endian integer, then its exact bytes; require the final lower-case hex digest to equal {}. {instructions} Recheck input digest and installed tool versions after setup. Record only each verified version token, not the whole command output; each token must be at most 64 ASCII characters drawn from letters, digits, dot, plus, hyphen and underscore. A cache hit must pass the same checks as a cold setup. Atomically write a UTF-8 JSON marker of at most 4096 bytes, only after success, with exactly these fields: version=1, session={session}, workspace_id={}, operation_id={}, repository_id={}, canonical_scope={scope}, workspace={workspace}, adapter={adapter}, inputs_sha256={}, tool_versions={{vp,pnpm,cargo,rustc,sccache}}, status=ready. Use null for inapplicable or absent tools. If setup fails, write no ready marker and give a bounded non-secret failure summary. If task outcome is lost, do not replay it blindly; reconcile the retained task and marker using the same operation ID.",
            serde_json::to_string(&self.repository_id)?,
            serde_json::to_string(&self.workspace_id)?,
            serde_json::to_string(&self.repository_id)?,
            self.inputs_sha256,
            serde_json::to_string(&self.workspace_id)?,
            serde_json::to_string(&self.operation_id)?,
            serde_json::to_string(&self.repository_id)?,
            serde_json::to_string(&self.inputs_sha256)?,
        ))
    }

    pub(crate) fn marker_path(&self) -> PathBuf {
        self.private_cache()
            .join(format!("{}.json", self.operation_id))
    }

    pub(crate) fn private_cache(&self) -> PathBuf {
        self.workspace
            .join(".temote-mcp/environment")
            .join(self.workspace_id.to_string())
    }

    pub(crate) fn shared_cache(&self) -> PathBuf {
        if self.workspace == self.canonical_scope {
            return self.private_cache().join("source-cache");
        }
        let digest = Sha256::digest(self.repository_id.logical_name().as_bytes());
        self.canonical_scope
            .join(".temote-mcp/environment/shared")
            .join(format!("{digest:x}"))
    }

    pub(crate) fn inspect_ready(&self, session: &Session) -> Result<ReadyStamp> {
        ensure!(
            self.session.matches(session),
            "preparation session instance changed"
        );
        self.validate_scope()?;
        self.inspect_cache_owner(&self.private_cache(), Some(self.workspace_id))?;
        self.inspect_cache_owner(&self.shared_cache(), None)?;
        for output in match self.adapter {
            Adapter::VpPnpm => vec![self.workspace.join("node_modules")],
            Adapter::CargoSccache => vec![self.private_cache().join("target")],
        } {
            if fs::symlink_metadata(&output).is_ok() {
                inspect_managed_path(&self.canonical_scope, &output)?;
                ensure!(output.is_dir(), "preparation output is not a directory");
            }
        }
        let path = self.marker_path();
        inspect_managed_path(&self.canonical_scope, &path)?;
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_MARKER_BYTES,
            "preparation marker is not a bounded regular file"
        );
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        ensure!(file.metadata()?.is_file(), "preparation marker changed");
        let mut bytes = Vec::new();
        file.take(MAX_MARKER_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_MARKER_BYTES,
            "preparation marker is too large"
        );
        let stamp: ReadyStamp =
            serde_json::from_slice(&bytes).context("invalid preparation marker")?;
        ensure!(
            stamp.status == PreparationPhase::Ready && stamp.version == PREPARATION_VERSION,
            "preparation marker is not ready"
        );
        ensure!(
            stamp.session == self.session
                && stamp.workspace_id == self.workspace_id
                && stamp.operation_id == self.operation_id
                && stamp.repository_id == self.repository_id
                && stamp.canonical_scope == self.canonical_scope
                && stamp.workspace == self.workspace
                && stamp.adapter == self.adapter
                && stamp.inputs_sha256 == self.inputs_sha256,
            "preparation marker does not match the operation and scope"
        );
        stamp.tool_versions.validate(self.adapter)?;
        Ok(stamp)
    }

    pub(crate) fn validate_scope(&self) -> Result<()> {
        ensure!(
            self.version == PREPARATION_VERSION,
            "unsupported preparation version"
        );
        ensure!(
            fs::canonicalize(&self.canonical_scope)? == self.canonical_scope,
            "canonical preparation scope changed"
        );
        ensure!(
            fs::canonicalize(&self.workspace)? == self.workspace
                && self.workspace.starts_with(&self.canonical_scope),
            "workspace scope changed"
        );
        ensure!(
            hash_inputs(&self.workspace, self.adapter)? == self.inputs_sha256,
            "preparation inputs changed"
        );
        Ok(())
    }

    fn inspect_cache_owner(&self, cache: &Path, workspace_id: Option<Uuid>) -> Result<()> {
        let owner_path = cache.join("owner.json");
        inspect_managed_path(&self.canonical_scope, &owner_path)?;
        let metadata = fs::symlink_metadata(&owner_path)?;
        ensure!(
            metadata.is_file() && metadata.len() <= 512,
            "cache owner is not a bounded regular file"
        );
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&owner_path)?;
        ensure!(file.metadata()?.is_file(), "cache owner changed");
        let mut bytes = Vec::new();
        file.take(513).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 512, "cache owner is too large");
        let owner: CacheOwner = serde_json::from_slice(&bytes).context("invalid cache owner")?;
        ensure!(
            owner
                == CacheOwner {
                    version: 1,
                    repository_id: self.repository_id.clone(),
                    workspace_id
                },
            "cache ownership changed"
        );
        Ok(())
    }
}

fn hash_inputs(workspace: &Path, adapter: Adapter) -> Result<String> {
    let mut digest = Sha256::new();
    for name in adapter.inputs() {
        let path = workspace.join(name);
        let metadata = fs::symlink_metadata(&path).with_context(|| format!("missing {name}"))?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_INPUT_BYTES,
            "preparation input is not a bounded regular file"
        );
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        ensure!(file.metadata()?.is_file(), "preparation input changed");
        let mut bytes = Vec::new();
        file.take(MAX_INPUT_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == metadata.len(),
            "preparation input changed"
        );
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(&bytes);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn inspect_managed_path(scope: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(scope)
        .context("marker escaped preparation scope")?;
    let mut current = scope.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            anyhow::bail!("unsafe marker path")
        };
        current.push(name);
        ensure!(
            !fs::symlink_metadata(&current)?.file_type().is_symlink(),
            "managed preparation path contains a symbolic link"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(scope: &Path, id: &str) -> Session {
        Session {
            id: id.into(),
            cwd: fs::canonicalize(scope).unwrap(),
            permitted_directories: vec![],
            started_at: 123,
            process_id: 456,
            permission_mode: PermissionMode::Agent,
            grants: Default::default(),
        }
    }

    fn bound_session(plan: &PreparationPlan) -> Session {
        session(&plan.canonical_scope, &plan.session.session_id)
    }

    fn fixture(adapter: Adapter) -> (tempfile::TempDir, PreparationPlan) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        for name in adapter.inputs() {
            fs::write(workspace.join(name), name).unwrap();
        }
        let plan = PreparationPlan::new(
            &session(root.path(), "session"),
            Uuid::new_v4(),
            Uuid::new_v4(),
            RepositoryId::new("github.com", "owner", "repo").unwrap(),
            &workspace,
            adapter,
        )
        .unwrap();
        (root, plan)
    }

    fn versions(adapter: Adapter) -> ToolVersions {
        match adapter {
            Adapter::VpPnpm => ToolVersions {
                vp: Some("1.0".into()),
                pnpm: Some("10.0".into()),
                cargo: None,
                rustc: None,
                sccache: None,
            },
            Adapter::CargoSccache => ToolVersions {
                vp: None,
                pnpm: None,
                cargo: Some("1.0".into()),
                rustc: Some("1.0".into()),
                sccache: None,
            },
        }
    }

    fn stamp(plan: &PreparationPlan) -> ReadyStamp {
        ReadyStamp {
            version: plan.version,
            session: plan.session.clone(),
            workspace_id: plan.workspace_id,
            operation_id: plan.operation_id,
            repository_id: plan.repository_id.clone(),
            canonical_scope: plan.canonical_scope.clone(),
            workspace: plan.workspace.clone(),
            adapter: plan.adapter,
            inputs_sha256: plan.inputs_sha256.clone(),
            tool_versions: versions(plan.adapter),
            status: PreparationPhase::Ready,
        }
    }

    fn write_owners(plan: &PreparationPlan) {
        for (cache, workspace_id) in [
            (plan.private_cache(), Some(plan.workspace_id)),
            (plan.shared_cache(), None),
        ] {
            fs::create_dir_all(&cache).unwrap();
            fs::write(
                cache.join("owner.json"),
                serde_json::to_vec(&CacheOwner {
                    version: 1,
                    repository_id: plan.repository_id.clone(),
                    workspace_id,
                })
                .unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn cold_and_hit_use_the_same_ready_stamp() {
        for adapter in [Adapter::VpPnpm, Adapter::CargoSccache] {
            let (_root, plan) = fixture(adapter);
            let cold = stamp(&plan);
            write_owners(&plan);
            fs::write(plan.marker_path(), serde_json::to_vec(&cold).unwrap()).unwrap();
            let hit = plan.inspect_ready(&bound_session(&plan)).unwrap();
            assert_eq!(cold, hit);
            assert!(plan.delegated_task().unwrap().contains("coding agent"));
        }
    }

    #[test]
    fn adapter_detection_requires_one_complete_pair() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(detect_adapter(root.path()).unwrap(), None);
        fs::write(root.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_adapter(root.path()).unwrap(), None);
        fs::write(root.path().join("Cargo.lock"), "").unwrap();
        assert_eq!(
            detect_adapter(root.path()).unwrap(),
            Some(Adapter::CargoSccache)
        );
        fs::write(root.path().join("package.json"), "{}").unwrap();
        fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(detect_adapter(root.path()).unwrap(), None);
    }

    #[test]
    fn foreign_cache_owner_blocks_ready_marker() {
        let (_root, plan) = fixture(Adapter::CargoSccache);
        write_owners(&plan);
        fs::write(
            plan.marker_path(),
            serde_json::to_vec(&stamp(&plan)).unwrap(),
        )
        .unwrap();
        assert!(plan.inspect_ready(&bound_session(&plan)).is_ok());
        fs::write(
            plan.shared_cache().join("owner.json"),
            serde_json::to_vec(&CacheOwner {
                version: 1,
                repository_id: RepositoryId::new("github.com", "other", "repo").unwrap(),
                workspace_id: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(plan.inspect_ready(&bound_session(&plan)).is_err());
    }

    #[test]
    fn stale_or_foreign_marker_is_never_ready() {
        let (_root, plan) = fixture(Adapter::VpPnpm);
        fs::create_dir_all(plan.private_cache()).unwrap();
        let mut foreign = stamp(&plan);
        foreign.operation_id = Uuid::new_v4();
        fs::write(plan.marker_path(), serde_json::to_vec(&foreign).unwrap()).unwrap();
        assert!(plan.inspect_ready(&bound_session(&plan)).is_err());
        let mut replaced = bound_session(&plan);
        replaced.started_at += 1;
        assert!(plan.inspect_ready(&replaced).is_err());
        fs::write(
            plan.marker_path(),
            serde_json::to_vec(&stamp(&plan)).unwrap(),
        )
        .unwrap();
        fs::write(plan.workspace.join("pnpm-lock.yaml"), "changed").unwrap();
        assert!(plan.inspect_ready(&bound_session(&plan)).is_err());
    }

    #[test]
    fn workspace_cache_is_private_and_repository_cache_is_stable() {
        let (root, first) = fixture(Adapter::CargoSccache);
        let second_workspace = root.path().join("sibling");
        fs::create_dir(&second_workspace).unwrap();
        for name in Adapter::CargoSccache.inputs() {
            fs::write(second_workspace.join(name), name).unwrap();
        }
        let second = PreparationPlan::new(
            &session(root.path(), "other"),
            Uuid::new_v4(),
            Uuid::new_v4(),
            first.repository_id.clone(),
            &second_workspace,
            Adapter::CargoSccache,
        )
        .unwrap();
        assert_eq!(first.shared_cache(), second.shared_cache());
        assert_ne!(first.private_cache(), second.private_cache());
        assert_ne!(first.marker_path(), second.marker_path());
        let scoped = PreparationPlan::new(
            &session(&first.workspace, "workspace-only"),
            Uuid::new_v4(),
            Uuid::new_v4(),
            first.repository_id.clone(),
            &first.workspace,
            Adapter::CargoSccache,
        )
        .unwrap();
        assert!(scoped.shared_cache().starts_with(&scoped.workspace));
    }

    #[test]
    fn failed_or_uncertain_marker_cannot_become_ready() {
        let (_root, plan) = fixture(Adapter::CargoSccache);
        fs::create_dir_all(plan.private_cache()).unwrap();
        for status in [
            PreparationPhase::Failed,
            PreparationPhase::Uncertain,
            PreparationPhase::InFlight,
        ] {
            let mut marker = stamp(&plan);
            marker.status = status;
            fs::write(plan.marker_path(), serde_json::to_vec(&marker).unwrap()).unwrap();
            assert!(plan.inspect_ready(&bound_session(&plan)).is_err());
        }
    }

    #[test]
    fn version_tokens_are_bounded_and_adapter_specific() {
        let (_root, plan) = fixture(Adapter::CargoSccache);
        fs::create_dir_all(plan.private_cache()).unwrap();
        let mut marker = stamp(&plan);
        marker.tool_versions.cargo = Some("cargo 1.9 (hash)".into());
        fs::write(plan.marker_path(), serde_json::to_vec(&marker).unwrap()).unwrap();
        assert!(plan.inspect_ready(&bound_session(&plan)).is_err());
        marker.tool_versions.cargo = Some("1.9.0".into());
        marker.tool_versions.pnpm = Some("10.0".into());
        fs::write(plan.marker_path(), serde_json::to_vec(&marker).unwrap()).unwrap();
        assert!(plan.inspect_ready(&bound_session(&plan)).is_err());
    }

    #[test]
    fn receipt_reconciles_without_blind_replay() {
        let (_root, plan) = fixture(Adapter::CargoSccache);
        let mut receipt = PreparationReceipt::new(plan.clone());
        receipt.begin().unwrap();
        assert!(receipt.begin().is_err());
        assert_eq!(
            receipt
                .reconcile(&bound_session(&plan), DelegatedExecution::Unknown)
                .unwrap(),
            PreparationPhase::Uncertain
        );
        assert!(receipt.begin().is_err());
        assert_eq!(
            receipt
                .reconcile(&bound_session(&plan), DelegatedExecution::Completed)
                .unwrap(),
            PreparationPhase::Failed
        );
        fs::create_dir_all(plan.private_cache()).unwrap();
        write_owners(&plan);
        fs::write(
            plan.marker_path(),
            serde_json::to_vec(&stamp(&plan)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            receipt
                .reconcile(&bound_session(&plan), DelegatedExecution::Completed)
                .unwrap(),
            PreparationPhase::Ready
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_marker_is_rejected() {
        use std::os::unix::fs::symlink;
        let (root, plan) = fixture(Adapter::VpPnpm);
        let link = root.path().join("link");
        symlink(&plan.workspace, &link).unwrap();
        assert!(
            PreparationPlan::new(
                &session(root.path(), "s"),
                Uuid::new_v4(),
                Uuid::new_v4(),
                plan.repository_id.clone(),
                &link,
                Adapter::VpPnpm
            )
            .is_ok()
        );
        fs::create_dir_all(plan.private_cache()).unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, serde_json::to_vec(&stamp(&plan)).unwrap()).unwrap();
        symlink(&outside, plan.marker_path()).unwrap();
        assert!(plan.inspect_ready(&bound_session(&plan)).is_err());
    }
}
