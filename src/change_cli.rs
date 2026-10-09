//! Typed local Change commands. Transport wiring belongs to cli.rs and MCP.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::change::{
    ChangeBase, ChangeScope, ChangeStore, ExecutionAttempt, InitialStart, WriterLease,
};
use crate::config;
use crate::delivery::{self, DeliveryObservation, DeliveryPlan, DeliveryStep};
use crate::observation;
use crate::session_source::RepositoryId;
use crate::vcs::{
    DelegatedSnapshotReport, JournalVcsObservationSink, SystemCommandRunner, VcsManager,
};

const MAX_PLAN_CHANGES: usize = 64;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ChangeBinding {
    owner_operation_id: Uuid,
    allocation_operation_id: Uuid,
    change_id: String,
    task_id: String,
    workspace_id: String,
}

fn binding_path(operation_id: Uuid) -> Result<PathBuf> {
    let directory = config::state_dir()?.join("changes/bindings");
    fs::create_dir_all(&directory)?;
    ensure!(
        !fs::symlink_metadata(&directory)?.file_type().is_symlink(),
        "Change binding directory is a link"
    );
    let directory = directory.canonicalize()?;
    #[cfg(unix)]
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    Ok(directory.join(format!("{operation_id}.json")))
}

/// Cross-process admission held through a bound task probe/control and its
/// snapshot reservation. A competing caller fails before accepting side effects.
pub(crate) struct BoundAdmission(File);

impl Drop for BoundAdmission {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: this guard owns the open descriptor.
            let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn try_bound_admission(path: &std::path::Path) -> Result<BoundAdmission> {
    use std::os::fd::AsRawFd;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "bound task admission is not a regular file"
    );
    // SAFETY: file owns a live descriptor; nonblocking acquisition cannot
    // stall the async executor or overwrite another process's authority.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "BOUND_TASK_BUSY: another task boundary is active; retry without a new operation_id"
    );
    Ok(BoundAdmission(file))
}

pub(crate) fn bound_task_admission(
    session: &config::Session,
    task_id: &str,
) -> Result<Option<BoundAdmission>> {
    let receipts = crate::repository_store::RepositoryStore::new()?;
    let Some(executor) = receipts.find_by_session_id(&session.id)? else {
        return Ok(None);
    };
    let Some(binding) = read_binding(executor.operation_id)? else {
        return Ok(None);
    };
    if binding.task_id != task_id {
        return Ok(None);
    }
    ensure!(
        executor
            .activated_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(session)),
        "bound task session instance changed"
    );
    Ok(Some(try_bound_admission(
        &binding_path(executor.operation_id)?.with_extension("admission.lock"),
    )?))
}

fn read_binding(operation_id: Uuid) -> Result<Option<ChangeBinding>> {
    let path = binding_path(operation_id)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        file.metadata()?.len() <= 2048,
        "Change binding exceeds bound"
    );
    let mut bytes = Vec::new();
    file.take(2049).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 2048, "Change binding exceeds bound");
    let binding: ChangeBinding = serde_json::from_slice(&bytes)?;
    ensure!(
        binding.allocation_operation_id == operation_id,
        "Change binding receipt mismatch"
    );
    Ok(Some(binding))
}

fn record_binding(binding: &ChangeBinding) -> Result<()> {
    let path = binding_path(binding.allocation_operation_id)?;
    if let Some(existing) = read_binding(binding.allocation_operation_id)? {
        ensure!(existing == *binding, "Change binding operation conflict");
        return Ok(());
    }
    let bytes = serde_json::to_vec(binding)?;
    ensure!(bytes.len() <= 2048, "Change binding exceeds bound");
    let directory = path.parent().context("Change binding parent missing")?;
    let temporary = directory.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    let linked = fs::hard_link(&temporary, &path);
    let _ = fs::remove_file(&temporary);
    match linked {
        Ok(()) => File::open(directory)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                read_binding(binding.allocation_operation_id)? == Some(binding.clone()),
                "Change binding operation conflict"
            );
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
pub(crate) const USAGE: &str = "Usage: temote-mcp change --provisioning-operation-id UUID --request JSON\nJSON action: create_provisioned, create_ready, list, get, allocate, bind_provisioned, start, append_execution, acquire_writer, release_writer, observe_task_revision, snapshot, reconcile_snapshot, verify, plan, deliver, reconcile_delivery, release_ready\n";

pub(crate) struct ChangeInvocation {
    pub provisioning_operation_id: Uuid,
    pub request: ChangeCommand,
}

pub(crate) fn parse(args: &[String]) -> std::result::Result<ChangeInvocation, String> {
    if args.len() != 4 || args[0] != "--provisioning-operation-id" || args[2] != "--request" {
        return Err(USAGE.to_owned());
    }
    if args[3].len() > 16 * 1024 {
        return Err("Change request exceeds 16 KiB".to_owned());
    }
    let provisioning_operation_id = Uuid::parse_str(&args[1])
        .map_err(|_| "provisioning operation ID must be a UUID".to_owned())?;
    let request =
        serde_json::from_str(&args[3]).map_err(|_| "invalid typed Change request".to_owned())?;
    Ok(ChangeInvocation {
        provisioning_operation_id,
        request,
    })
}

pub(crate) async fn run_managed(invocation: ChangeInvocation) -> Result<()> {
    let result = execute_managed(invocation).await?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

pub(crate) async fn execute_managed(invocation: ChangeInvocation) -> Result<Value> {
    let receipt = crate::repository_store::RepositoryStore::new()?
        .read(invocation.provisioning_operation_id)?
        .context("managed provisioning receipt not found")?;
    ensure!(
        receipt.phase == crate::repository_store::ProvisioningPhase::WorkspaceReady,
        "managed workspace is not ready"
    );
    ensure!(
        receipt.request.vcs != crate::session_source::VcsPreference::Git,
        "Git Change snapshot capability unsupported"
    );
    ensure!(
        receipt
            .request
            .base
            .as_deref()
            .is_none_or(|base| base == "main"),
        "source branch Change capability unsupported"
    );
    let session = config::read_session_metadata(&receipt.session_id).await?;
    ensure!(
        receipt
            .activated_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(&session)),
        "provisioning receipt does not own this session instance"
    );
    let roots = crate::named_roots::NamedRoots::from_env()?;
    let root = roots
        .canonical_root(&receipt.root_name)
        .context("provisioning named root unavailable")?;
    ensure!(
        receipt.canonical_root.as_deref() == Some(root),
        "provisioning named root changed"
    );
    let workspace = root.join(crate::workspace_provisioning::workspace_relative(&receipt));
    ensure!(
        workspace.canonicalize()? == session.cwd,
        "session workspace differs from provisioning receipt"
    );
    crate::workspace_provisioning::inspect_ready(root, &receipt)?;
    let actor = observation::ActorRef {
        transport: "change-cli".to_owned(),
        principal: None,
    };
    let context = ChangeContext {
        session: &session,
        repository: &receipt.request.repository,
        actor: &actor,
        delivery_observer: None,
    };
    execute(&context, invocation.request).await
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ChangeCommand {
    CreateProvisioned {
        provisioning_operation_id: Uuid,
        task_id: String,
        parent_task_id: Option<String>,
    },
    Create {
        operation_id: Uuid,
        task_id: String,
        parent_task_id: Option<String>,
        parent_change_id: Option<String>,
        base: ChangeBase,
    },
    CreateReady {
        operation_id: Uuid,
        allocation_operation_id: Uuid,
        task_operation_id: Uuid,
        parent_task_id: Option<String>,
        parent_change_id: Option<String>,
        base: ChangeBase,
    },
    List,
    Get {
        change_id: String,
    },
    Allocate {
        change_id: String,
        expected_revision: u64,
        operation_id: Uuid,
    },
    BindProvisioned {
        change_id: String,
        expected_revision: u64,
        provisioning_operation_id: Uuid,
    },
    Start {
        change_id: String,
        expected_revision: u64,
        operation_id: Uuid,
        task: String,
        model: String,
        effort: String,
    },
    AppendExecution {
        change_id: String,
        expected_revision: u64,
        execution: ExecutionAttempt,
    },
    AcquireWriter {
        change_id: String,
        expected_revision: u64,
        execution_id: String,
    },
    ReleaseWriter {
        change_id: String,
        expected_revision: u64,
        writer: WriterLease,
    },
    ObserveTaskRevision {
        change_id: String,
        expected_revision: u64,
        task_record_revision: u64,
    },
    Snapshot {
        change_id: String,
        expected_revision: u64,
        operation_id: Uuid,
        execution_id: String,
    },
    ReconcileSnapshot {
        change_id: String,
        expected_revision: u64,
        operation_id: Uuid,
    },
    Verify {
        change_id: String,
        expected_revision: u64,
        materialized_revision: String,
        passed: bool,
    },
    Plan {
        change_ids: Vec<String>,
    },
    Deliver {
        change_ids: Vec<String>,
        change_id: String,
        expected_revision: u64,
        operation_id: Uuid,
        model: String,
        effort: String,
    },
    ReconcileDelivery {
        change_ids: Vec<String>,
        change_id: String,
        expected_revision: u64,
        operation_id: Uuid,
        model: String,
        effort: String,
    },
    ReleaseReady {
        change_id: String,
    },
}

/// Read-only remote observation supplied by the transport's authorized
/// observer. CLI input cannot forge a PR receipt into Completed delivery.
pub(crate) trait DeliveryObserver: Sync {
    fn observe(&self, step: &DeliveryStep) -> Result<DeliveryObservation>;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteObservationEnvelope {
    operation_id: Uuid,
    repository_id: String,
    change_id: String,
    observation: DeliveryObservation,
}

pub(crate) struct ChangeContext<'a> {
    pub session: &'a config::Session,
    /// This identity must come from an authoritative repository resolver or
    /// validated managed-provisioning receipt, not a raw caller string.
    pub repository: &'a RepositoryId,
    pub actor: &'a observation::ActorRef,
    pub delivery_observer: Option<&'a dyn DeliveryObserver>,
}

fn store(context: &ChangeContext<'_>) -> Result<ChangeStore> {
    let canonical_root = context.session.cwd.canonicalize()?;
    ensure!(
        canonical_root == context.session.cwd,
        "session workspace is not canonical"
    );
    let scope = ChangeScope {
        session_id: context.session.id.clone(),
        session_started_at: context.session.started_at,
        session_process_id: context.session.process_id,
        canonical_root,
        repository_id: context.repository.logical_name(),
    };
    ChangeStore::open(&config::state_dir()?.join("changes"), scope)
}

fn owner_receipt(
    context: &ChangeContext<'_>,
) -> Result<crate::repository_store::ProvisioningReceipt> {
    let receipt = crate::repository_store::RepositoryStore::new()?
        .find_by_session_id(&context.session.id)?
        .context("Change owner has no managed provisioning receipt")?;
    ensure!(
        receipt
            .activated_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(context.session))
            && receipt.request.repository == *context.repository
            && receipt.phase == crate::repository_store::ProvisioningPhase::WorkspaceReady,
        "Change owner session is not the accepted managed instance"
    );
    Ok(receipt)
}

async fn allocation_session(
    context: &ChangeContext<'_>,
    operation_id: Uuid,
) -> Result<(
    crate::repository_store::ProvisioningReceipt,
    config::Session,
)> {
    let owner = owner_receipt(context)?;
    let receipt = crate::repository_store::RepositoryStore::new()?
        .read(operation_id)?
        .context("managed allocation receipt not found")?;
    ensure!(
        receipt.phase == crate::repository_store::ProvisioningPhase::WorkspaceReady
            && receipt.request.repository == *context.repository
            && receipt.root_name == owner.root_name
            && receipt.canonical_root == owner.canonical_root,
        "managed allocation is outside the Change owner repository/root"
    );
    let session = config::read_session_metadata(&receipt.session_id).await?;
    ensure!(
        receipt
            .activated_owner
            .as_ref()
            .is_some_and(|bound| bound.matches(&session))
            && config::session_is_active(&session.id).await?,
        "managed execution session is no longer the allocated full instance"
    );
    let root = receipt
        .canonical_root
        .as_deref()
        .context("managed root unavailable")?;
    ensure!(
        root.join(crate::workspace_provisioning::workspace_relative(&receipt))
            .canonicalize()?
            == session.cwd,
        "managed execution session workspace differs from allocation"
    );
    crate::workspace_provisioning::inspect_ready(root, &receipt)?;
    Ok((receipt, session))
}

async fn session_for_change(
    context: &ChangeContext<'_>,
    change: &crate::change::ChangeRecord,
) -> Result<config::Session> {
    let operation_id = change
        .provisioning_operation_id
        .context("Change has no managed allocation receipt")?;
    let (receipt, session) = allocation_session(context, operation_id).await?;
    ensure!(
        change.workspace_id.as_deref() == Some(receipt.workspace_id.to_string().as_str()),
        "Change workspace differs from managed allocation"
    );
    Ok(session)
}

async fn vcs_for_change(
    context: &ChangeContext<'_>,
    change: &crate::change::ChangeRecord,
) -> Result<VcsManager<SystemCommandRunner, JournalVcsObservationSink>> {
    let session = session_for_change(context, change).await?;
    let receipt = owner_receipt(context)?;
    let root = receipt.canonical_root.context("managed root unavailable")?;
    Ok(VcsManager::open_for_session(
        &session.cwd,
        &root.join(".temote-mcp/workspaces"),
        &session,
    )?)
}

/// Called by the shared task boundary only for an exact, explicitly bound
/// managed Change. The executor's provisioning receipt identifies its own
/// session; the owner receipt identifies the separate Change graph authority.
pub(crate) async fn observe_bound_task_view(
    session: &config::Session,
    backend: crate::orchestration::Backend,
    view: &Value,
) -> Result<()> {
    let Some(task_id) = view.get("task_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let receipts = crate::repository_store::RepositoryStore::new()?;
    let Some(executor_receipt) = receipts.find_by_session_id(&session.id)? else {
        return Ok(());
    };
    ensure!(
        executor_receipt
            .activated_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(session)),
        "managed execution session changed"
    );
    let Some(binding) = read_binding(executor_receipt.operation_id)? else {
        return Ok(());
    };
    if binding.task_id != task_id {
        // A delegated observer or unrelated task does not acquire a Change.
        return Ok(());
    }
    ensure!(
        view.get("cloud").and_then(Value::as_bool) != Some(true),
        "hosted execution cannot mutate a local managed Change workspace"
    );
    #[cfg(feature = "network")]
    ensure!(
        backend != crate::orchestration::Backend::DevinCloud,
        "hosted execution cannot mutate a local managed Change workspace"
    );
    ensure!(
        binding.workspace_id == executor_receipt.workspace_id.to_string(),
        "Change binding workspace mismatch"
    );
    let owner_receipt = receipts
        .read(binding.owner_operation_id)?
        .context("Change owner receipt missing")?;
    ensure!(
        owner_receipt.phase == crate::repository_store::ProvisioningPhase::WorkspaceReady
            && owner_receipt.request.repository == executor_receipt.request.repository
            && owner_receipt.root_name == executor_receipt.root_name
            && owner_receipt.canonical_root == executor_receipt.canonical_root,
        "Change binding crossed a repository or managed root"
    );
    let owner_session = config::read_session_metadata(&owner_receipt.session_id).await?;
    ensure!(
        owner_receipt
            .activated_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(&owner_session))
            && config::session_is_active(&owner_session.id).await?,
        "Change owner full session instance is unavailable"
    );
    let actor = observation::ActorRef {
        transport: "change-task-boundary".to_owned(),
        principal: None,
    };
    let context = ChangeContext {
        session: &owner_session,
        repository: &owner_receipt.request.repository,
        actor: &actor,
        delivery_observer: None,
    };
    let store = store(&context)?;
    let mut change = store.get(&binding.change_id)?;
    ensure!(
        change.task_id == task_id
            && change.workspace_id.as_deref() == Some(binding.workspace_id.as_str())
            && change.provisioning_operation_id == Some(binding.allocation_operation_id),
        "Change task binding differs from durable allocation"
    );
    let execution_id = view
        .pointer("/execution/id")
        .and_then(Value::as_str)
        .context("task view has no canonical execution.id")?;
    let generation = view
        .pointer("/execution/generation")
        .and_then(Value::as_u64)
        .context("task view has no execution generation")?;
    let task_revision = view
        .get("revision")
        .and_then(Value::as_u64)
        .context("task view has no revision")?;
    ensure!(
        generation > 0 && task_revision > 0,
        "task execution or revision is invalid"
    );
    ensure!(
        task_revision >= change.task_record_revision,
        "task boundary view is older than the Change record"
    );
    if task_revision == change.task_record_revision
        && change.executions.last().is_some_and(|attempt| {
            attempt.execution_id == execution_id && attempt.generation == generation
        })
    {
        return Ok(());
    }
    ensure!(
        task_revision > change.task_record_revision,
        "task execution conflicts at the same TaskRecord revision"
    );
    let attempt = ExecutionAttempt {
        execution_id: execution_id.to_owned(),
        backend: backend.name().to_owned(),
        executor: change
            .executions
            .iter()
            .find(|attempt| attempt.execution_id == execution_id)
            .map(|attempt| attempt.executor.clone())
            .unwrap_or_else(|| backend.name().to_owned()),
        generation,
    };
    change = store.append_execution(&change.change_id, change.revision, attempt)?;
    if let Some(writer) = &change.writer
        && writer.execution_id != execution_id
    {
        change = store.handoff_writer(
            &change.change_id,
            change.revision,
            Some(writer),
            execution_id,
        )?;
    }
    let status = view
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let terminal = matches!(
        status,
        "completed" | "failed" | "interrupted" | "retryable_failed"
    );
    // A running or waiting writer may still edit the working copy. Its views
    // remain observational; only a quiescent boundary can start a jj helper.
    if !terminal {
        return Ok(());
    }
    if change.writer.is_none() {
        change = store.handoff_writer(&change.change_id, change.revision, None, execution_id)?;
    }
    let task_uuid = Uuid::parse_str(task_id).context("bound task identity is not a UUID")?;
    let snapshot_operation_id =
        boundary_snapshot_operation_id(task_uuid, &change.change_id, execution_id, task_revision);
    let model = view
        .get("model")
        .and_then(Value::as_str)
        .context("task model missing")?;
    let effort = view
        .get("effort")
        .and_then(Value::as_str)
        .context("task effort missing")?;
    let Some(result) = delegated_snapshot(
        &context,
        &store,
        &change,
        snapshot_operation_id,
        execution_id,
        task_revision,
        model,
        effort,
    )
    .await?
    else {
        return Ok(());
    };
    let workspace = change
        .workspace_id
        .as_deref()
        .context("Change has no workspace")?;
    ensure!(
        result.observation.task_id.as_deref() == Some(task_id)
            && result.observation.execution_id.as_deref() == Some(execution_id)
            && result.observation.workspace_id == workspace,
        "automatic snapshot correlation mismatch"
    );
    change = store.observe_snapshot(
        &change.change_id,
        change.revision,
        snapshot_operation_id,
        &result.observation.logical_change_id,
        &result.observation.after_revision,
    )?;
    change = store.observe_task_revision(&change.change_id, change.revision, task_revision)?;
    if let Some(verification) = view.get("verification") {
        let passed = match verification.get("status").and_then(Value::as_str) {
            Some("passed") => Some(true),
            Some("failed") => Some(false),
            _ => None,
        };
        if let Some(passed) = passed
            && verification.get("stale").and_then(Value::as_bool) == Some(false)
            && verification.pointer("/target/kind").and_then(Value::as_str) == Some("commit")
            && verification
                .pointer("/target/commit")
                .and_then(Value::as_str)
                == change.materialized_revision.as_deref()
        {
            change = store.verify(
                &change.change_id,
                change.revision,
                change
                    .materialized_revision
                    .as_deref()
                    .context("snapshot revision missing")?,
                passed,
            )?;
        }
    }
    if let Some(writer) = &change.writer {
        store.release_writer(&change.change_id, change.revision, writer)?;
    }
    Ok(())
}

/// Generic task starts are denied in a bound execution workspace. Typed
/// Change and delivery helpers use the internal pre-start admission hook.
/// Ordinary sessions retain the generic task contract.
pub(crate) async fn authorize_bound_start(session: &config::Session) -> Result<()> {
    authorize_unbound_start(session)
}

pub(crate) fn authorize_unbound_start(session: &config::Session) -> Result<()> {
    let receipts = crate::repository_store::RepositoryStore::new()?;
    let Some(executor) = receipts.find_by_session_id(&session.id)? else {
        return Ok(());
    };
    let Some(_) = read_binding(executor.operation_id)? else {
        return Ok(());
    };
    ensure!(
        executor
            .activated_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(session)),
        "bound Change execution session changed"
    );
    anyhow::bail!("bound Change task start requires a server-owned typed admission")
}

/// A bound workspace cannot accept a new mutating turn while a jj helper or
/// its observation outbox has an unresolved receipt. Read durable state again
/// immediately before generic control dispatch, after any approval wait.
pub(crate) async fn authorize_bound_control(session: &config::Session, action: &str) -> Result<()> {
    if !matches!(action, "resume" | "steer") {
        return Ok(());
    }
    let receipts = crate::repository_store::RepositoryStore::new()?;
    let Some(executor) = receipts.find_by_session_id(&session.id)? else {
        return Ok(());
    };
    let Some(binding) = read_binding(executor.operation_id)? else {
        return Ok(());
    };
    ensure!(
        executor
            .activated_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(session))
            && executor.workspace_id.to_string() == binding.workspace_id,
        "bound Change execution session changed"
    );
    let owner = receipts
        .read(binding.owner_operation_id)?
        .context("bound Change owner receipt missing")?;
    ensure!(
        owner.request.repository == executor.request.repository
            && owner.root_name == executor.root_name
            && owner.canonical_root == executor.canonical_root,
        "bound Change owner changed"
    );
    let owner_session = config::read_session_metadata(&owner.session_id).await?;
    ensure!(
        owner
            .activated_owner
            .as_ref()
            .is_some_and(|identity| identity.matches(&owner_session))
            && config::session_is_active(&owner_session.id).await?,
        "bound Change owner session unavailable"
    );
    let actor = observation::ActorRef {
        transport: "change-control-admission".to_owned(),
        principal: None,
    };
    let context = ChangeContext {
        session: &owner_session,
        repository: &owner.request.repository,
        actor: &actor,
        delivery_observer: None,
    };
    let change = store(&context)?.get(&binding.change_id)?;
    ensure!(
        change.task_id == binding.task_id
            && change.workspace_id.as_deref() == Some(binding.workspace_id.as_str())
            && change.provisioning_operation_id == Some(executor.operation_id),
        "bound Change correlation changed"
    );
    let vcs = vcs_for_change(&context, &change).await?;
    ensure_bound_control_snapshot_clear(
        action,
        &change,
        vcs.workspace_has_unreconciled_snapshots(&binding.workspace_id)?,
    )
}

fn ensure_bound_control_snapshot_clear(
    action: &str,
    change: &crate::change::ChangeRecord,
    unreconciled: bool,
) -> Result<()> {
    if !matches!(action, "resume" | "steer") {
        return Ok(());
    }
    ensure!(
        !unreconciled
            && change
                .initial_start
                .as_ref()
                .is_none_or(|start| !start.snapshot_dispatch_attempted || start.snapshot_verified),
        "bound Change snapshot is pending or requires reconciliation"
    );
    Ok(())
}

/// Recheck the exact managed executor and Change owner binding at the child
/// startup boundary. This grants no generic task-start capability.
pub(crate) fn bound_delivery_admitted(
    session: &config::Session,
    change: &crate::change::ChangeRecord,
) -> Result<()> {
    let receipts = crate::repository_store::RepositoryStore::new()?;
    let executor = receipts
        .find_by_session_id(&session.id)?
        .context("delivery executor receipt missing")?;
    let binding =
        read_binding(executor.operation_id)?.context("delivery Change binding missing")?;
    let owner = receipts
        .read(binding.owner_operation_id)?
        .context("delivery Change owner receipt missing")?;
    ensure!(
        executor
            .activated_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(session))
            && executor.phase == crate::repository_store::ProvisioningPhase::WorkspaceReady
            && executor.workspace_id.to_string() == binding.workspace_id
            && executor.operation_id == binding.allocation_operation_id
            && executor.request.repository == owner.request.repository
            && owner.phase == crate::repository_store::ProvisioningPhase::WorkspaceReady
            && executor.root_name == owner.root_name
            && executor.canonical_root == owner.canonical_root
            && session.cwd
                == executor
                    .canonical_root
                    .as_deref()
                    .context("delivery named root missing")?
                    .join(crate::workspace_provisioning::workspace_relative(&executor))
                    .canonicalize()?
            && binding.change_id == change.change_id
            && binding.task_id == change.task_id
            && change.workspace_id.as_deref() == Some(binding.workspace_id.as_str())
            && change.provisioning_operation_id == Some(executor.operation_id)
            && change.scope.session_id == owner.session_id
            && owner
                .activated_owner
                .as_ref()
                .is_some_and(
                    |identity| identity.started_at == change.scope.session_started_at
                        && identity.process_id == change.scope.session_process_id
                        && identity.cwd == change.scope.canonical_root
                        && identity.session_id == change.scope.session_id
                ),
        "delivery executor/owner full-instance binding changed"
    );
    Ok(())
}

fn managed_base(
    store: &ChangeStore,
    change: &crate::change::ChangeRecord,
) -> Result<Option<String>> {
    managed_base_for(store, &change.base)
}

fn managed_base_for(store: &ChangeStore, base: &ChangeBase) -> Result<Option<String>> {
    match base {
        ChangeBase::OriginMain => Ok(Some("main".to_owned())),
        ChangeBase::Change(parent_id) => {
            let parent = store.get(parent_id)?;
            let receipt = parent
                .delivery
                .as_ref()
                .context("base_change has no reconciled delivery")?;
            ensure!(
                receipt.state == delivery::DeliveryState::Completed
                    && receipt.branch_revision == parent.materialized_revision
                    && receipt.pull_request.is_some(),
                "base_change is not delivered at its current revision"
            );
            Ok(Some(format!("temote/{parent_id}")))
        }
    }
}

fn selected_plan(store: &ChangeStore, ids: &[String]) -> Result<DeliveryPlan> {
    ensure!(
        !ids.is_empty() && ids.len() <= MAX_PLAN_CHANGES,
        "Change plan must contain 1..=64 IDs"
    );
    let changes = ids
        .iter()
        .map(|id| store.get(id))
        .collect::<Result<Vec<_>>>()?;
    delivery::plan(&changes)
}

fn projection(change: &crate::change::ChangeRecord) -> Value {
    json!({
        "change_id": change.change_id,
        "owner_session_id": change.scope.session_id,
        "repository_id": change.scope.repository_id,
        "task_id": change.task_id,
        "parent_task_id": change.parent_task_id,
        "parent_change_id": change.parent_change_id,
        "base": change.base,
        "workspace_id": change.workspace_id,
        "allocation_pending": change.allocation_pending,
        "execution": change.executions.last(),
        "initial_start": change.initial_start,
        "writer": change.writer,
        "logical_change_id": change.logical_change_id,
        "materialized_revision": change.materialized_revision,
        "task_record_revision": change.task_record_revision,
        "verification": change.verification,
        "delivery": change.delivery,
        "revision": change.revision,
    })
}

fn boundary_snapshot_operation_id(
    task_id: Uuid,
    change_id: &str,
    execution_id: &str,
    task_revision: u64,
) -> Uuid {
    Uuid::new_v5(
        &task_id,
        format!("change-snapshot-v1:{change_id}:{execution_id}:{task_revision}").as_bytes(),
    )
}

fn validate_task_id(task_id: &str) -> Result<()> {
    Uuid::parse_str(task_id).context("task ID must be a UUID")?;
    Ok(())
}

fn initial_start_fingerprint(
    change_id: &str,
    operation_id: Uuid,
    task: &str,
    model: &str,
    effort: &str,
) -> Result<String> {
    let bytes = serde_json::to_vec(&json!({
        "change_id": change_id, "operation_id": operation_id,
        "task": task, "model": model, "effort": effort,
    }))?;
    Ok(Uuid::new_v5(&operation_id, &bytes).to_string())
}

fn delegated_snapshot_prompt(
    operation_id: Uuid,
    task_id: &str,
    workspace_id: &str,
    execution_id: Option<&str>,
) -> String {
    format!(
        r#"Perform one Temote Change jj snapshot in this managed execution workspace. Before jj status, inspect @ with jj --ignore-working-copy log -r @ -T 'change_id ++ "\n" ++ commit_id ++ "\n" ++ conflict ++ "\n" ++ empty ++ "\n"' and inspect the current jj operation ID with jj --ignore-working-copy op log -n 1. Then run jj status exactly once and inspect the same fields and operation ID again with --ignore-working-copy. Do not edit files, commit, push, or change branches. If any command fails, the logical change changes, or the after state is conflicted, report blocked. Only after both read-backs succeed, return a native structured report with status completed and summary containing compact JSON with exactly operation_id, task_id, workspace_id, execution_id, before, after. Each state contains logical_change_id, materialized_revision (full commit ID), vcs_operation_id, conflicted (boolean), empty (boolean). Use operation_id {}, task_id {}, workspace_id {}, execution_id {} literally. Report observed values only; do not include credentials or command output."#,
        operation_id,
        task_id,
        workspace_id,
        execution_id
            .map(|id| format!("\"{id}\""))
            .unwrap_or_else(|| "null".to_owned())
    )
}

async fn launch_initial_snapshot(
    store: &ChangeStore,
    change: &crate::change::ChangeRecord,
    start: &InitialStart,
    execution_session: &config::Session,
    model: &str,
    effort: &str,
) -> Result<Value> {
    let args = json!({
        "operation_id": start.snapshot_operation_id,
        "task": delegated_snapshot_prompt(start.snapshot_operation_id, &change.task_id,
            change.workspace_id.as_deref().context("Change workspace missing")?, Some(&start.execution_id)),
        "model": model, "effort": effort,
    });
    let actor = observation::ActorRef {
        transport: "change-snapshot".to_owned(),
        principal: None,
    };
    crate::orchestration::invoke_codex_task_start_with_admission(
        &args,
        execution_session,
        &crate::codex_app_server::TaskStartOrigin::Generic,
        &actor,
        None,
        || {
            let current = store.get(&change.change_id)?;
            ensure!(
                initial_snapshot_admitted(&current, change, start),
                "initial snapshot reservation changed before delegated startup"
            );
            Ok(())
        },
    )
    .await
}

fn initial_snapshot_admitted(
    current: &crate::change::ChangeRecord,
    reserved: &crate::change::ChangeRecord,
    start: &InitialStart,
) -> bool {
    current.initial_start.as_ref() == Some(start)
        && start.snapshot_dispatch_attempted
        && !start.snapshot_verified
        && current.executions.is_empty()
        && current.writer.is_none()
        && current.workspace_id == reserved.workspace_id
        && current.task_id == reserved.task_id
        && current.scope == reserved.scope
}

fn initial_snapshot_needs_dispatch(start: &InitialStart) -> bool {
    !start.snapshot_dispatch_attempted
}

// Keep each typed authority and request component explicit at this boundary.
#[allow(clippy::too_many_arguments)]
async fn delegated_snapshot(
    context: &ChangeContext<'_>,
    store: &ChangeStore,
    change: &crate::change::ChangeRecord,
    operation_id: Uuid,
    execution_id: &str,
    task_revision: u64,
    model: &str,
    effort: &str,
) -> Result<Option<crate::vcs::SnapshotResult>> {
    let session = session_for_change(context, change).await?;
    let vcs = vcs_for_change(context, change).await?;
    let workspace = change
        .workspace_id
        .as_deref()
        .context("Change workspace missing")?;
    let new = vcs.reserve_delegated_snapshot(workspace, operation_id, Some(execution_id))?;
    if !new
        && let Some(result) =
            vcs.delegated_snapshot_result(workspace, operation_id, Some(execution_id))?
    {
        return Ok(Some(result));
    }
    let helper_id = crate::codex_app_server::task_id_for_operation(&session, operation_id)?;
    let args = json!({
        "operation_id": operation_id,
        "task": delegated_snapshot_prompt(operation_id, &change.task_id, workspace,
            Some(execution_id)),
        "model": model, "effort": effort,
    });
    let view = if new {
        let actor = observation::ActorRef {
            transport: "change-snapshot".to_owned(),
            principal: None,
        };
        crate::orchestration::invoke_codex_task_start_with_admission(
            &args,
            &session,
            &crate::codex_app_server::TaskStartOrigin::Generic,
            &actor,
            None,
            || {
                let current = store.get(&change.change_id)?;
                ensure!(
                    current.task_id == change.task_id
                        && current.workspace_id == change.workspace_id
                        && current.scope == change.scope
                        && current
                            .writer
                            .as_ref()
                            .is_none_or(|writer| writer.execution_id == execution_id)
                        && current
                            .executions
                            .last()
                            .is_some_and(|attempt| attempt.execution_id == execution_id)
                        && current.task_record_revision <= task_revision,
                    "delegated snapshot admission changed before startup"
                );
                Ok(())
            },
        )
        .await?
    } else {
        match crate::orchestration::invoke(
            crate::orchestration::Backend::Codex,
            crate::orchestration::Operation::TaskGet,
            &json!({"task_id":helper_id}),
            &session,
            context.actor,
            None,
        )
        .await
        {
            Ok(view) => view,
            Err(error) => {
                let retained = crate::codex_app_server::task_start_receipt_if_retained(
                    &args,
                    &session,
                    &crate::codex_app_server::TaskStartOrigin::Generic,
                )?;
                anyhow::bail!(
                    "delegated snapshot reconciliation_required: retained task read failed ({error}); receipt {}",
                    if retained.is_some() {
                        "exists"
                    } else {
                        "missing"
                    }
                );
            }
        }
    };
    ensure!(
        view.get("task_id").and_then(Value::as_str) == Some(helper_id.to_string().as_str()),
        "delegated snapshot helper identity mismatch"
    );
    if new || view.get("status").and_then(Value::as_str) != Some("completed") {
        ensure!(
            new || !matches!(
                view.get("status").and_then(Value::as_str),
                Some("failed" | "interrupted" | "retryable_failed" | "reconciliation_required")
            ),
            "delegated snapshot helper did not complete; reconciliation required"
        );
        return Ok(None);
    }
    ensure!(
        view.get("report_source").and_then(Value::as_str) == Some("native_structured_output")
            && view.get("report_status").and_then(Value::as_str) == Some("valid")
            && view.pointer("/report/status").and_then(Value::as_str) == Some("completed"),
        "delegated snapshot has no completed native structured report"
    );
    let summary = view
        .pointer("/report/summary")
        .and_then(Value::as_str)
        .context("delegated snapshot report missing")?;
    ensure!(
        summary.len() <= 2048,
        "delegated snapshot report exceeds bound"
    );
    let report: DelegatedSnapshotReport =
        serde_json::from_str(summary).context("invalid delegated snapshot report")?;
    Ok(Some(vcs.import_delegated_snapshot(
        workspace,
        operation_id,
        Some(execution_id),
        &report,
    )?))
}

fn delivery_observer_admitted(
    current: &crate::change::ChangeRecord,
    operation_id: Uuid,
    observation_operation_id: Uuid,
) -> bool {
    current.writer.is_none()
        && current.delivery.as_ref().is_some_and(|receipt| {
            receipt.operation_id == operation_id
                && receipt.observation_operation_id == Some(observation_operation_id)
        })
}

fn initial_workspace_tasks_idle(listed: &Value, task_id: &str) -> Result<()> {
    ensure!(
        listed.get("truncated").and_then(Value::as_bool) == Some(false),
        "cannot prove initial Change admission from truncated task list"
    );
    let backends = listed
        .get("backends")
        .and_then(Value::as_object)
        .context("shared task-list backend status missing")?;
    #[cfg(feature = "network")]
    let required = ["codex", "devin_acp", "opencode", "devin_cloud"];
    #[cfg(not(feature = "network"))]
    let required = ["codex", "devin_acp"];
    for backend in required {
        let status = backends
            .get(backend)
            .context("task-list backend unavailable")?;
        ensure!(
            status.get("status").and_then(Value::as_str) == Some("ok")
                && status.get("skipped").and_then(Value::as_u64) == Some(0),
            "cannot prove initial Change admission for {backend}"
        );
    }
    let tasks = listed
        .get("tasks")
        .and_then(Value::as_array)
        .context("retained task list unavailable")?;
    ensure!(
        !tasks
            .iter()
            .any(|view| view.get("task_id").and_then(Value::as_str) == Some(task_id)),
        "Change task already exists before initial snapshot"
    );
    ensure!(
        !tasks.iter().any(|view| !matches!(
            view.get("status").and_then(Value::as_str),
            Some("completed" | "failed" | "interrupted" | "retryable_failed")
        )),
        "execution workspace already has an active task"
    );
    Ok(())
}

// Keep each typed authority and request component explicit at this boundary.
#[allow(clippy::too_many_arguments)]
async fn start_change_task(
    context: &ChangeContext<'_>,
    store: &ChangeStore,
    change_id: &str,
    expected_revision: u64,
    operation_id: Uuid,
    task: &str,
    model: &str,
    effort: &str,
) -> Result<Value> {
    let mut change = store.get(change_id)?;
    ensure!(
        change.revision == expected_revision
            || change
                .initial_start
                .as_ref()
                .is_some_and(|start| start.operation_id == operation_id),
        "Change record revision conflict"
    );
    let execution_session = session_for_change(context, &change).await?;
    let task_id = crate::codex_app_server::task_id_for_operation(&execution_session, operation_id)?;
    ensure!(
        change.task_id == task_id.to_string(),
        "Change task ID does not match backend operation and execution session"
    );
    let allocation_operation_id = change
        .provisioning_operation_id
        .context("Change allocation missing")?;
    let binding = read_binding(allocation_operation_id)?.context("Change binding missing")?;
    ensure!(
        binding.change_id == change_id
            && binding.task_id == change.task_id
            && binding.workspace_id == change.workspace_id.as_deref().unwrap_or_default(),
        "Change binding does not match reserved start"
    );
    let fingerprint = initial_start_fingerprint(change_id, operation_id, task, model, effort)?;
    let start = InitialStart {
        operation_id,
        request_fingerprint: fingerprint,
        snapshot_operation_id: Uuid::new_v5(&operation_id, b"temote-change-initial-snapshot-v1"),
        execution_id: crate::orchestration::outcome::execution_id(task_id, 1).to_string(),
        snapshot_dispatch_attempted: false,
        snapshot_verified: false,
        vcs_operation_id: None,
    };
    let was_reserved = change.initial_start.is_some();
    if !was_reserved {
        let listed =
            crate::orchestration::task_list(&json!({"limit":128}), &execution_session).await?;
        initial_workspace_tasks_idle(&listed, &change.task_id)?;
    }
    change = store.reserve_initial_start(change_id, change.revision, start.clone())?;
    let mut start = change
        .initial_start
        .clone()
        .context("reserved start missing")?;
    let snapshot_task_id = crate::codex_app_server::task_id_for_operation(
        &execution_session,
        start.snapshot_operation_id,
    )?;
    if !start.snapshot_verified {
        let vcs = vcs_for_change(context, &change).await?;
        let workspace = change
            .workspace_id
            .clone()
            .context("Change workspace missing")?;
        let newly_attempted = if initial_snapshot_needs_dispatch(&start) {
            vcs.reserve_delegated_snapshot(
                &workspace,
                start.snapshot_operation_id,
                Some(&start.execution_id),
            )?
        } else {
            false
        };
        if initial_snapshot_needs_dispatch(&start) {
            change = store.mark_initial_snapshot_attempt(
                change_id,
                change.revision,
                start.snapshot_operation_id,
            )?;
            start = change
                .initial_start
                .clone()
                .context("snapshot attempt missing")?;
        }
        let view = if newly_attempted {
            launch_initial_snapshot(store, &change, &start, &execution_session, model, effort)
                .await?
        } else {
            match crate::orchestration::invoke(
                crate::orchestration::Backend::Codex,
                crate::orchestration::Operation::TaskGet,
                &json!({"task_id":snapshot_task_id}),
                &execution_session,
                context.actor,
                None,
            )
            .await
            {
                Ok(view) => view,
                Err(error) => {
                    let exact = json!({
                        "operation_id":start.snapshot_operation_id,
                        "task":delegated_snapshot_prompt(start.snapshot_operation_id, &change.task_id,
                            change.workspace_id.as_deref().context("Change workspace missing")?, Some(&start.execution_id)),
                        "model":model, "effort":effort,
                    });
                    let retained = crate::codex_app_server::task_start_receipt_if_retained(
                        &exact,
                        &execution_session,
                        &crate::codex_app_server::TaskStartOrigin::Generic,
                    )?;
                    anyhow::bail!(
                        "initial snapshot reconciliation_required: dispatch was attempted, task_get failed ({error}), retained receipt {}",
                        if retained.is_some() {
                            "exists"
                        } else {
                            "missing"
                        }
                    );
                }
            }
        };
        ensure!(
            view.get("task_id").and_then(Value::as_str)
                == Some(snapshot_task_id.to_string().as_str()),
            "initial snapshot task ID mismatch"
        );
        if newly_attempted || view.get("status").and_then(Value::as_str) != Some("completed") {
            ensure!(
                newly_attempted
                    || !matches!(
                        view.get("status").and_then(Value::as_str),
                        Some(
                            "failed"
                                | "interrupted"
                                | "retryable_failed"
                                | "reconciliation_required"
                        )
                    ),
                "initial snapshot did not complete; reconcile retained snapshot task before Change start"
            );
            return Ok(json!({"state":"pending_snapshot", "change_id":change_id,
                "task_id": change.task_id, "snapshot_task_id":snapshot_task_id,
                "snapshot_operation_id":start.snapshot_operation_id}));
        }
        ensure!(
            view.get("report_source").and_then(Value::as_str) == Some("native_structured_output")
                && view.get("report_status").and_then(Value::as_str) == Some("valid")
                && view.pointer("/report/status").and_then(Value::as_str) == Some("completed"),
            "initial snapshot has no completed native structured report"
        );
        let summary = view
            .pointer("/report/summary")
            .and_then(Value::as_str)
            .context("initial snapshot report summary missing")?;
        let report: DelegatedSnapshotReport =
            serde_json::from_str(summary).context("invalid initial snapshot report")?;
        ensure!(
            report.operation_id == start.snapshot_operation_id
                && report.task_id == change.task_id
                && Some(report.workspace_id.as_str()) == change.workspace_id.as_deref()
                && report.execution_id.as_deref() == Some(start.execution_id.as_str()),
            "initial snapshot correlation mismatch"
        );
        let imported = vcs.import_delegated_snapshot(
            &workspace,
            start.snapshot_operation_id,
            Some(&start.execution_id),
            &report,
        )?;
        change = store.verify_initial_snapshot(
            change_id,
            change.revision,
            start.snapshot_operation_id,
            &imported.observation.logical_change_id,
            &imported.observation.after_revision,
            &imported.observation.vcs_operation_id,
        )?;
    }
    let start = change
        .initial_start
        .clone()
        .context("reserved start missing")?;
    ensure!(
        vcs_for_change(context, &change)
            .await?
            .delegated_snapshot_result(
                change
                    .workspace_id
                    .as_deref()
                    .context("Change workspace missing")?,
                start.snapshot_operation_id,
                Some(&start.execution_id),
            )?
            .is_some(),
        "initial Change snapshot has no completed VCS receipt"
    );
    if change.executions.is_empty() {
        change = store.append_execution(
            change_id,
            change.revision,
            ExecutionAttempt {
                execution_id: start.execution_id.clone(),
                backend: "codex".to_owned(),
                executor: "codex".to_owned(),
                generation: 1,
            },
        )?;
    }
    ensure!(
        change
            .executions
            .last()
            .is_some_and(|e| e.execution_id == start.execution_id && e.generation == 1),
        "Change execution reservation changed"
    );
    if change.writer.is_none() {
        change = store.handoff_writer(change_id, change.revision, None, &start.execution_id)?;
    }
    ensure!(
        change
            .writer
            .as_ref()
            .is_some_and(|writer| writer.execution_id == start.execution_id),
        "Change writer reservation changed"
    );
    let args = json!({"operation_id":operation_id, "task":task, "model":model, "effort":effort});
    let actor = observation::ActorRef {
        transport: "change-start".to_owned(),
        principal: None,
    };
    let view = crate::orchestration::invoke_codex_task_start_with_admission(
        &args,
        &execution_session,
        &crate::codex_app_server::TaskStartOrigin::Generic,
        &actor,
        None,
        || {
            let latest = store.get(change_id)?;
            ensure!(
                latest
                    .initial_start
                    .as_ref()
                    .is_some_and(|s| s == &start.clone()),
                "initial Change start reservation changed before backend startup"
            );
            ensure!(
                latest
                    .writer
                    .as_ref()
                    .is_some_and(|w| w.execution_id == start.execution_id),
                "Change writer changed before backend startup"
            );
            Ok(())
        },
    )
    .await?;
    ensure!(
        view.get("task_id").and_then(Value::as_str) == Some(change.task_id.as_str()),
        "started Change task ID mismatch"
    );
    if let Some(generation) = view
        .pointer("/execution/generation")
        .and_then(Value::as_u64)
        && generation > 0
    {
        ensure!(
            generation == 1
                && view.pointer("/execution/id").and_then(Value::as_str)
                    == Some(start.execution_id.as_str()),
            "started Change execution generation mismatch"
        );
    }
    Ok(json!({"state":"started", "change":projection(&store.get(change_id)?), "task":view}))
}

async fn current_task_view(
    context: &ChangeContext<'_>,
    change: &crate::change::ChangeRecord,
) -> Result<Value> {
    let execution_session = session_for_change(context, change).await?;
    let execution = change
        .executions
        .last()
        .context("Change has no execution attempt")?;
    let backend = match execution.backend.as_str() {
        "codex" => crate::orchestration::Backend::Codex,
        "devin_acp" => crate::orchestration::Backend::DevinAcp,
        #[cfg(feature = "network")]
        "opencode" => crate::orchestration::Backend::OpenCode,
        #[cfg(feature = "network")]
        "devin_cloud" => crate::orchestration::Backend::DevinCloud,
        _ => anyhow::bail!("task backend capability unsupported"),
    };
    let view = crate::orchestration::invoke(
        backend,
        crate::orchestration::Operation::TaskGet,
        &json!({"task_id": change.task_id}),
        &execution_session,
        context.actor,
        None,
    )
    .await?;
    ensure!(
        view.get("task_id").and_then(Value::as_str) == Some(change.task_id.as_str()),
        "task view identity mismatch"
    );
    ensure!(
        view.pointer("/execution/id").and_then(Value::as_str)
            == Some(execution.execution_id.as_str()),
        "task execution generation changed"
    );
    ensure!(
        view.get("revision").and_then(Value::as_u64).is_some(),
        "task view has no revision"
    );
    Ok(view)
}

async fn current_task_revision(
    context: &ChangeContext<'_>,
    change: &crate::change::ChangeRecord,
) -> Result<u64> {
    current_task_view(context, change)
        .await?
        .get("revision")
        .and_then(Value::as_u64)
        .context("task view has no revision")
}

async fn ensure_plan_tasks_current(
    context: &ChangeContext<'_>,
    store: &ChangeStore,
    ids: &[String],
) -> Result<()> {
    for id in ids {
        let change = store.get(id)?;
        ensure!(
            current_task_revision(context, &change).await? == change.task_record_revision,
            "Change verification is stale against TaskRecord"
        );
    }
    Ok(())
}

async fn bind_provisioned(
    context: &ChangeContext<'_>,
    store: &ChangeStore,
    change_id: &str,
    rev: u64,
    operation_id: Uuid,
) -> Result<Value> {
    let owner = owner_receipt(context)?;
    let (receipt, session) = allocation_session(context, operation_id).await?;
    if operation_id == owner.operation_id {
        ensure!(
            change_id == receipt.change_id.to_string(),
            "owner provisioned Change identity mismatch"
        );
    }
    ensure!(
        receipt.request.vcs != crate::session_source::VcsPreference::Git,
        "Git allocation capability unsupported"
    );
    let current = store.get(change_id)?;
    let binding = ChangeBinding {
        owner_operation_id: owner.operation_id,
        allocation_operation_id: operation_id,
        change_id: change_id.to_owned(),
        task_id: current.task_id.clone(),
        workspace_id: receipt.workspace_id.to_string(),
    };
    ensure!(
        receipt.request.base.as_deref().unwrap_or("main")
            == managed_base(store, &current)?.as_deref().unwrap_or("main"),
        "base_change conflicts with managed allocation base"
    );
    ensure!(
        current.allocation_operation_id.is_none()
            || current.allocation_operation_id == Some(operation_id),
        "Change allocation operation conflict"
    );
    if current.workspace_id.as_deref() == Some(receipt.workspace_id.to_string().as_str()) {
        ensure!(
            current.allocation_operation_id == Some(operation_id)
                && current.provisioning_operation_id == Some(operation_id),
            "bound workspace has no matching allocation receipt"
        );
        let root = owner.canonical_root.context("managed root unavailable")?;
        VcsManager::open_for_session(&session.cwd, &root.join(".temote-mcp/workspaces"), &session)?
            .register_provisioned_snapshot_workspace(&receipt, &current.task_id)?;
        record_binding(&binding)?;
        return Ok(serde_json::to_value(current)?);
    }
    let intent = if current.allocation_pending {
        ensure!(
            current.allocation_operation_id == Some(operation_id),
            "pending allocation belongs to another operation"
        );
        current
    } else {
        store.allocation_intent(change_id, rev, operation_id)?
    };
    let root = owner.canonical_root.context("managed root unavailable")?;
    let vcs =
        VcsManager::open_for_session(&session.cwd, &root.join(".temote-mcp/workspaces"), &session)?;
    let change = store.get(change_id)?;
    vcs.register_provisioned_snapshot_workspace(&receipt, &change.task_id)?;
    record_binding(&binding)?;
    let updated = store.bind_managed_workspace(
        change_id,
        intent.revision,
        operation_id,
        &receipt.workspace_id.to_string(),
    )?;
    Ok(serde_json::to_value(updated)?)
}

async fn snapshot(
    context: &ChangeContext<'_>,
    store: &ChangeStore,
    change_id: &str,
    rev: u64,
    operation_id: Uuid,
    execution_id: Option<&str>,
    _reconcile: bool,
) -> Result<Value> {
    let change = store.get(change_id)?;
    ensure!(change.revision == rev, "Change record revision conflict");
    let workspace_id = change
        .workspace_id
        .as_deref()
        .context("Change has no managed workspace")?;
    let current_execution = change
        .executions
        .last()
        .context("Change has no execution")?;
    if let Some(requested) = execution_id {
        ensure!(
            requested == current_execution.execution_id,
            "snapshot execution is not current"
        );
    }
    let executor = session_for_change(context, &change).await?;
    let _admission = bound_task_admission(&executor, &change.task_id)?;
    let view = current_task_view(context, &change).await?;
    ensure!(
        matches!(
            view.get("status").and_then(Value::as_str),
            Some("completed" | "failed" | "interrupted" | "retryable_failed")
        ),
        "snapshot requires a quiescent task boundary"
    );
    ensure!(change.writer.is_none(), "snapshot writer is still active");
    let model = view
        .get("model")
        .and_then(Value::as_str)
        .context("task model missing")?;
    let effort = view
        .get("effort")
        .and_then(Value::as_str)
        .context("task effort missing")?;
    let task_revision = view
        .get("revision")
        .and_then(Value::as_u64)
        .context("task revision missing")?;
    let Some(result) = delegated_snapshot(
        context,
        store,
        &change,
        operation_id,
        &current_execution.execution_id,
        task_revision,
        model,
        effort,
    )
    .await?
    else {
        return Ok(
            json!({"state":"pending_snapshot", "operation_id":operation_id,
            "change_id":change_id}),
        );
    };
    ensure!(
        result.observation.workspace_id == workspace_id
            && result.observation.task_id.as_deref() == Some(change.task_id.as_str()),
        "snapshot task/workspace correlation mismatch"
    );
    ensure!(
        result.observation.execution_id.as_deref() == Some(current_execution.execution_id.as_str()),
        "snapshot execution correlation mismatch"
    );
    if let Some(observed_execution) = &result.observation.execution_id {
        ensure!(
            change
                .executions
                .iter()
                .any(|e| &e.execution_id == observed_execution),
            "snapshot inferred unknown execution"
        );
    }
    let updated = store.observe_snapshot(
        change_id,
        rev,
        operation_id,
        &result.observation.logical_change_id,
        &result.observation.after_revision,
    )?;
    Ok(json!({"change": updated, "snapshot": result}))
}

// Keep each typed authority and request component explicit at this boundary.
#[allow(clippy::too_many_arguments)]
async fn reconcile_delegated_remote(
    context: &ChangeContext<'_>,
    store: &ChangeStore,
    change_id: &str,
    expected_revision: u64,
    operation_id: Uuid,
    plan: &DeliveryPlan,
    step: &DeliveryStep,
    model: &str,
    effort: &str,
) -> Result<Value> {
    let execution_session = session_for_change(context, &store.get(change_id)?).await?;
    ensure!(
        context.repository.host() == "github.com",
        "remote PR observation capability unsupported for this repository host"
    );
    if let Some(receipt) = store.get(change_id)?.delivery
        && receipt.is_terminal()
    {
        ensure!(
            receipt.operation_id == operation_id,
            "delivery operation conflict"
        );
        return Ok(serde_json::to_value(receipt)?);
    }
    let observation_operation_id =
        Uuid::new_v5(&operation_id, b"temote-read-only-delivery-observation-v1");
    let receipt = delivery::prepare_observation(
        store,
        change_id,
        expected_revision,
        operation_id,
        observation_operation_id,
    )?;
    let task_id = if let Some(task_id) = receipt.observation_task_id {
        task_id
    } else {
        let request = json!({
            "operation_id": observation_operation_id.to_string(),
            "task": format!(
                "Read-only Temote delivery reconciliation for repository {} and Change {}. Inspect the live remote head ref {}, base ref {}, exact head revision {}, and matching GitHub PR. Do not mutate any local or remote state. In the native structured task report, set status completed only after observation and put a compact JSON object in summary with exactly operation_id, repository_id, change_id, observation. observation contains branch_revision (full observed head revision or null) and pull_request (number, head_ref, base_ref, revision, url, stack_parent_pr_number, stack_linked, or null). Use operation_id {} and repository_id {} literally. If any remote read is uncertain or unavailable, report blocked and do not claim a PR.",
                context.repository.logical_name(), step.change_id, step.head_ref, step.base_ref,
                step.revision, observation_operation_id, context.repository.logical_name()),
            "model": model,
            "effort": effort,
        });
        let view = crate::orchestration::invoke_codex_task_start_with_admission(
            &request,
            &execution_session,
            &crate::codex_app_server::TaskStartOrigin::Generic,
            context.actor,
            None,
            || {
                let current = store.get(change_id)?;
                ensure!(
                    delivery_observer_admitted(&current, operation_id, observation_operation_id),
                    "read-only delivery observation admission changed"
                );
                Ok(())
            },
        )
        .await?;
        let id = view
            .get("task_id")
            .and_then(Value::as_str)
            .context("read-only observation task accepted without task ID")?;
        delivery::record_observation_task(
            store,
            change_id,
            store.get(change_id)?.revision,
            operation_id,
            observation_operation_id,
            id,
        )?;
        id.to_owned()
    };
    let view = crate::orchestration::invoke(
        crate::orchestration::Backend::Codex,
        crate::orchestration::Operation::TaskGet,
        &json!({"task_id": task_id}),
        &execution_session,
        context.actor,
        None,
    )
    .await?;
    ensure!(
        view.get("task_id").and_then(Value::as_str) == Some(task_id.as_str()),
        "observation task identity mismatch"
    );
    if view.get("status").and_then(Value::as_str) != Some("completed") {
        let status = view.get("status").and_then(Value::as_str);
        let state = if matches!(
            status,
            Some("failed" | "interrupted" | "reconciliation_required")
        ) {
            "reconciliation_required"
        } else {
            "observation_pending"
        };
        return Ok(json!({"state":state, "task_id":task_id,
            "task_status":view.get("status")}));
    }
    ensure!(
        view.get("report_source").and_then(Value::as_str) == Some("native_structured_output")
            && view.get("report_status").and_then(Value::as_str) == Some("valid")
            && view.pointer("/report/status").and_then(Value::as_str) == Some("completed"),
        "observation task has no valid completed structured report"
    );
    let summary = view
        .pointer("/report/summary")
        .and_then(Value::as_str)
        .context("observation summary missing")?;
    ensure!(summary.len() <= 1200, "observation summary exceeds bound");
    let observed: RemoteObservationEnvelope =
        serde_json::from_str(summary).context("observation summary is not a typed receipt")?;
    ensure!(
        observed.operation_id == observation_operation_id
            && observed.repository_id == context.repository.logical_name()
            && observed.change_id == change_id,
        "remote observation identity mismatch"
    );
    if let Some(pr) = &observed.observation.pull_request {
        ensure!(
            pr.url
                == format!(
                    "https://github.com/{}/{}/pull/{}",
                    context.repository.owner(),
                    context.repository.name(),
                    pr.number
                ),
            "observed PR URL does not belong to repository"
        );
    }
    Ok(serde_json::to_value(delivery::reconcile(
        store,
        change_id,
        store.get(change_id)?.revision,
        plan,
        operation_id,
        observed.observation,
    )?)?)
}

pub(crate) async fn execute(context: &ChangeContext<'_>, command: ChangeCommand) -> Result<Value> {
    let current = config::read_session_metadata(&context.session.id).await?;
    ensure!(
        serde_json::to_value(&current)? == serde_json::to_value(context.session)?,
        "session instance changed"
    );
    ensure!(
        config::session_is_active(&context.session.id).await?,
        "session is not active"
    );
    let store = store(context)?;
    match command {
        ChangeCommand::CreateProvisioned {
            provisioning_operation_id,
            task_id,
            parent_task_id,
        } => {
            validate_task_id(&task_id)?;
            if let Some(parent) = &parent_task_id {
                validate_task_id(parent)?;
            }
            let receipt = crate::repository_store::RepositoryStore::new()?
                .read(provisioning_operation_id)?
                .context("managed provisioning receipt not found")?;
            ensure!(
                receipt.session_id == context.session.id,
                "provisioning session mismatch"
            );
            ensure!(
                receipt.request.repository == *context.repository,
                "provisioning repository mismatch"
            );
            ensure!(
                receipt.phase == crate::repository_store::ProvisioningPhase::WorkspaceReady,
                "managed workspace is not ready"
            );
            Ok(serde_json::to_value(store.create_provisioned(
                receipt.change_id,
                &task_id,
                parent_task_id.as_deref(),
            )?)?)
        }
        ChangeCommand::Create {
            operation_id,
            task_id,
            parent_task_id,
            parent_change_id,
            base,
        } => {
            validate_task_id(&task_id)?;
            if let Some(parent) = &parent_task_id {
                validate_task_id(parent)?;
            }
            Ok(serde_json::to_value(store.create_idempotent(
                operation_id,
                &task_id,
                parent_task_id.as_deref(),
                parent_change_id.as_deref(),
                base,
            )?)?)
        }
        ChangeCommand::CreateReady {
            operation_id,
            allocation_operation_id,
            task_operation_id,
            parent_task_id,
            parent_change_id,
            base,
        } => {
            if let Some(parent) = &parent_task_id {
                validate_task_id(parent)?;
            }
            ensure!(
                parent_change_id
                    .as_deref()
                    .is_none_or(|parent| matches!(&base, ChangeBase::Change(id) if id == parent)),
                "base_change and parent_change_id conflict"
            );
            let owner = owner_receipt(context)?;
            let (allocation, executor) =
                allocation_session(context, allocation_operation_id).await?;
            ensure!(
                allocation.request.vcs != crate::session_source::VcsPreference::Git,
                "Git Change snapshot capability unsupported"
            );
            ensure!(
                allocation.request.base.as_deref().unwrap_or("main")
                    == managed_base_for(&store, &base)?
                        .as_deref()
                        .unwrap_or("main"),
                "ready allocation base conflicts with Change base"
            );
            let task_id =
                crate::codex_app_server::task_id_for_operation(&executor, task_operation_id)?;
            let created = if allocation_operation_id == owner.operation_id {
                ensure!(
                    operation_id == owner.operation_id
                        && parent_change_id.is_none()
                        && matches!(base, ChangeBase::OriginMain)
                        && allocation.change_id == owner.change_id,
                    "owner ready allocation must preserve its provisioned Change identity"
                );
                store.create_provisioned(
                    owner.change_id,
                    &task_id.to_string(),
                    parent_task_id.as_deref(),
                )?
            } else {
                store.create_idempotent(
                    operation_id,
                    &task_id.to_string(),
                    parent_task_id.as_deref(),
                    parent_change_id.as_deref(),
                    base,
                )?
            };
            bind_provisioned(
                context,
                &store,
                &created.change_id,
                created.revision,
                allocation_operation_id,
            )
            .await
        }
        ChangeCommand::List => {
            let changes = store.list()?;
            ensure!(
                changes.len() <= 128,
                "Change list exceeds bounded local projection"
            );
            Ok(json!({"changes": changes.iter().map(projection).collect::<Vec<_>>()}))
        }
        ChangeCommand::Get { change_id } => Ok(projection(&store.get(&change_id)?)),
        ChangeCommand::Allocate {
            change_id,
            expected_revision,
            operation_id,
        } => {
            let owner = owner_receipt(context)?;
            let current = store.get(&change_id)?;
            ensure!(
                current.revision == expected_revision
                    || current.allocation_operation_id == Some(operation_id),
                "Change record revision conflict"
            );
            ensure!(
                current.allocation_operation_id.is_none()
                    || current.allocation_operation_id == Some(operation_id),
                "Change allocation operation conflict"
            );
            if current.workspace_id.is_some() {
                ensure!(
                    current.provisioning_operation_id == Some(operation_id),
                    "Change allocation receipt conflict"
                );
                return bind_provisioned(
                    context,
                    &store,
                    &change_id,
                    current.revision,
                    operation_id,
                )
                .await;
            }
            let base = managed_base(&store, &current)?;
            if current.allocation_operation_id.is_none() {
                store.allocation_intent(&change_id, expected_revision, operation_id)?;
            }
            let request = if operation_id == owner.operation_id {
                ensure!(
                    current.change_id == owner.change_id.to_string()
                        && matches!(current.base, ChangeBase::OriginMain),
                    "owner provisioning receipt cannot allocate another Change"
                );
                owner.request.clone()
            } else {
                crate::repository_store::ManagedRequest {
                    repository: context.repository.clone(),
                    base,
                    vcs: owner.request.vcs,
                }
            };
            let receipts = crate::repository_store::RepositoryStore::new()?;
            if let Some(accepted) = receipts.read(operation_id)? {
                ensure!(
                    accepted.request == request
                        && accepted.root_name == owner.root_name
                        && accepted.canonical_root == owner.canonical_root,
                    "accepted sibling allocation conflicts with Change owner"
                );
            }
            if receipts.read(operation_id)?.as_ref().is_none_or(|receipt| {
                receipt.phase != crate::repository_store::ProvisioningPhase::WorkspaceReady
            }) {
                let backend = crate::session_control::SessionBackend::local_control().await?;
                backend.start_managed(operation_id, request).await?;
            }
            let receipt = receipts
                .read(operation_id)?
                .context("managed allocator returned no receipt")?;
            if receipt.phase == crate::repository_store::ProvisioningPhase::WorkspaceReady {
                bind_provisioned(
                    context,
                    &store,
                    &change_id,
                    store.get(&change_id)?.revision,
                    operation_id,
                )
                .await
            } else {
                Ok(json!({
                    "change": store.get(&change_id)?,
                    "allocation": crate::supervisor::managed_provisioning_view(&receipt)
                }))
            }
        }
        ChangeCommand::BindProvisioned {
            change_id,
            expected_revision,
            provisioning_operation_id,
        } => {
            bind_provisioned(
                context,
                &store,
                &change_id,
                expected_revision,
                provisioning_operation_id,
            )
            .await
        }
        ChangeCommand::Start {
            change_id,
            expected_revision,
            operation_id,
            task,
            model,
            effort,
        } => {
            start_change_task(
                context,
                &store,
                &change_id,
                expected_revision,
                operation_id,
                &task,
                &model,
                &effort,
            )
            .await
        }
        ChangeCommand::AppendExecution {
            change_id,
            expected_revision,
            execution,
        } => {
            let mut candidate = store.get(&change_id)?;
            candidate.executions.push(execution.clone());
            let view = current_task_view(context, &candidate).await?;
            ensure!(
                view.pointer("/execution/generation")
                    .and_then(Value::as_u64)
                    == Some(execution.generation),
                "task execution generation mismatch"
            );
            Ok(serde_json::to_value(store.append_execution(
                &change_id,
                expected_revision,
                execution,
            )?)?)
        }
        ChangeCommand::AcquireWriter {
            change_id,
            expected_revision,
            execution_id,
        } => {
            let change = store.get(&change_id)?;
            ensure!(
                change
                    .executions
                    .last()
                    .is_some_and(|e| e.execution_id == execution_id),
                "writer execution is not current"
            );
            current_task_view(context, &change).await?;
            Ok(serde_json::to_value(store.handoff_writer(
                &change_id,
                expected_revision,
                None,
                &execution_id,
            )?)?)
        }
        ChangeCommand::ReleaseWriter {
            change_id,
            expected_revision,
            writer,
        } => Ok(serde_json::to_value(store.release_writer(
            &change_id,
            expected_revision,
            &writer,
        )?)?),
        ChangeCommand::ObserveTaskRevision {
            change_id,
            expected_revision,
            task_record_revision,
        } => {
            let change = store.get(&change_id)?;
            ensure!(
                current_task_revision(context, &change).await? == task_record_revision,
                "requested TaskRecord revision is stale"
            );
            Ok(serde_json::to_value(store.observe_task_revision(
                &change_id,
                expected_revision,
                task_record_revision,
            )?)?)
        }
        ChangeCommand::Snapshot {
            change_id,
            expected_revision,
            operation_id,
            execution_id,
        } => {
            let change = store.get(&change_id)?;
            ensure!(
                change
                    .executions
                    .last()
                    .is_some_and(|e| e.execution_id == execution_id),
                "snapshot execution is not current"
            );
            snapshot(
                context,
                &store,
                &change_id,
                expected_revision,
                operation_id,
                Some(&execution_id),
                false,
            )
            .await
        }
        ChangeCommand::ReconcileSnapshot {
            change_id,
            expected_revision,
            operation_id,
        } => {
            snapshot(
                context,
                &store,
                &change_id,
                expected_revision,
                operation_id,
                None,
                true,
            )
            .await
        }
        ChangeCommand::Verify {
            change_id,
            expected_revision,
            materialized_revision,
            passed,
        } => {
            let change = store.get(&change_id)?;
            let view = current_task_view(context, &change).await?;
            ensure!(
                view.get("revision").and_then(Value::as_u64) == Some(change.task_record_revision),
                "verification TaskRecord revision is stale"
            );
            let verification = view
                .get("verification")
                .context("task verification unavailable")?;
            ensure!(
                verification.get("status").and_then(Value::as_str)
                    == Some(if passed { "passed" } else { "failed" })
                    && verification.get("stale").and_then(Value::as_bool) == Some(false)
                    && verification.pointer("/target/kind").and_then(Value::as_str)
                        == Some("commit")
                    && verification
                        .pointer("/target/commit")
                        .and_then(Value::as_str)
                        == Some(materialized_revision.as_str()),
                "TaskRecord has no current verification for the exact materialized revision"
            );
            Ok(serde_json::to_value(store.verify(
                &change_id,
                expected_revision,
                &materialized_revision,
                passed,
            )?)?)
        }
        ChangeCommand::Plan { change_ids } => {
            ensure_plan_tasks_current(context, &store, &change_ids).await?;
            Ok(serde_json::to_value(selected_plan(&store, &change_ids)?)?)
        }
        ChangeCommand::Deliver {
            change_ids,
            change_id,
            expected_revision,
            operation_id,
            model,
            effort,
        } => {
            ensure_plan_tasks_current(context, &store, &change_ids).await?;
            let plan = selected_plan(&store, &change_ids)?;
            let execution_session = session_for_change(context, &store.get(&change_id)?).await?;
            let receipt = crate::orchestration::change_delivery_start(
                &store,
                &change_id,
                expected_revision,
                &plan,
                operation_id,
                &execution_session,
                context.actor,
                &model,
                &effort,
            )
            .await?;
            Ok(json!({"plan": plan, "receipt": receipt}))
        }
        ChangeCommand::ReconcileDelivery {
            change_ids,
            change_id,
            expected_revision,
            operation_id,
            model,
            effort,
        } => {
            let plan = selected_plan(&store, &change_ids)?;
            let step = plan
                .steps
                .iter()
                .find(|s| s.change_id == change_id)
                .context("Change absent from plan")?;
            if let Some(observer) = context.delivery_observer {
                let observed = observer.observe(step)?;
                Ok(serde_json::to_value(delivery::reconcile(
                    &store,
                    &change_id,
                    expected_revision,
                    &plan,
                    operation_id,
                    observed,
                )?)?)
            } else {
                reconcile_delegated_remote(
                    context,
                    &store,
                    &change_id,
                    expected_revision,
                    operation_id,
                    &plan,
                    step,
                    &model,
                    &effort,
                )
                .await
            }
        }
        ChangeCommand::ReleaseReady { change_id } => {
            let change = store.get(&change_id)?;
            let workspace = change
                .workspace_id
                .as_deref()
                .context("Change has no workspace")?;
            let vcs = vcs_for_change(context, &change).await?;
            let ready = store.release_ready(&change_id)?
                && !vcs.workspace_has_unreconciled_snapshots(workspace)?;
            Ok(json!({"change_id": change_id, "release_ready": ready}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_change() -> (tempfile::TempDir, ChangeStore, crate::change::ChangeRecord) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let store = ChangeStore::open(
            &root.join("changes"),
            ChangeScope {
                session_id: "owner".into(),
                session_started_at: 1,
                session_process_id: 2,
                canonical_root: root,
                repository_id: "github.com/o/r".into(),
            },
        )
        .unwrap();
        let created = store
            .create_idempotent(
                Uuid::new_v4(),
                &Uuid::new_v4().to_string(),
                None,
                None,
                ChangeBase::OriginMain,
            )
            .unwrap();
        let bound = store
            .bind_workspace(&created.change_id, created.revision, "workspace-a")
            .unwrap();
        (temp, store, bound)
    }

    #[test]
    fn ready_task_identity_is_derived_from_full_executor_instance() {
        let (temp, store, _) = fixture_change();
        let operation = Uuid::new_v4();
        let mut session = config::Session {
            id: "executor".into(),
            cwd: temp.path().canonicalize().unwrap(),
            started_at: 10,
            process_id: 20,
            permission_mode: config::PermissionMode::Agent,
            permitted_directories: vec![temp.path().canonicalize().unwrap()],
            grants: config::SessionGrants::default(),
        };
        let first_id = crate::codex_app_server::task_id_for_operation(&session, operation).unwrap();
        let create_op = Uuid::new_v4();
        let first = store
            .create_idempotent(
                create_op,
                &first_id.to_string(),
                None,
                None,
                ChangeBase::OriginMain,
            )
            .unwrap();
        assert_eq!(
            store
                .create_idempotent(
                    create_op,
                    &first_id.to_string(),
                    None,
                    None,
                    ChangeBase::OriginMain
                )
                .unwrap()
                .change_id,
            first.change_id
        );
        session.started_at += 1;
        let replacement =
            crate::codex_app_server::task_id_for_operation(&session, operation).unwrap();
        assert_ne!(first_id, replacement);
        assert!(
            store
                .create_idempotent(
                    create_op,
                    &replacement.to_string(),
                    None,
                    None,
                    ChangeBase::OriginMain
                )
                .is_err()
        );
    }

    #[test]
    fn helper_admission_requires_exact_reserved_receipts_and_idle_workspace() {
        let (_temp, store, bound) = fixture_change();
        let start = InitialStart {
            operation_id: Uuid::new_v4(),
            request_fingerprint: "request".into(),
            snapshot_operation_id: Uuid::new_v4(),
            execution_id: Uuid::new_v4().to_string(),
            snapshot_dispatch_attempted: false,
            snapshot_verified: false,
            vcs_operation_id: None,
        };
        let reserved = store
            .reserve_initial_start(&bound.change_id, bound.revision, start.clone())
            .unwrap();
        assert!(initial_snapshot_needs_dispatch(&start));
        assert!(!initial_snapshot_admitted(&reserved, &reserved, &start));
        let attempted = store
            .mark_initial_snapshot_attempt(
                &bound.change_id,
                reserved.revision,
                start.snapshot_operation_id,
            )
            .unwrap();
        let start = attempted.initial_start.as_ref().unwrap().clone();
        assert!(!initial_snapshot_needs_dispatch(&start));
        assert!(initial_snapshot_admitted(&attempted, &attempted, &start));
        let mut other = start.clone();
        other.snapshot_operation_id = Uuid::new_v4();
        assert!(!initial_snapshot_admitted(&attempted, &attempted, &other));
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
        assert!(!initial_snapshot_admitted(&verified, &attempted, &start));

        let delivery_op = Uuid::new_v4();
        let observation_op = Uuid::new_v4();
        let observing = store
            .update(&bound.change_id, verified.revision, |record| {
                record.delivery = Some(delivery::DeliveryReceipt {
                    operation_id: delivery_op,
                    plan_fingerprint: "plan".into(),
                    state: delivery::DeliveryState::Accepted,
                    delegated_task_id: None,
                    branch_revision: None,
                    pull_request: None,
                    observation_operation_id: Some(observation_op),
                    observation_task_id: None,
                });
                Ok(())
            })
            .unwrap();
        assert!(delivery_observer_admitted(
            &observing,
            delivery_op,
            observation_op
        ));
        assert!(!delivery_observer_admitted(
            &observing,
            Uuid::new_v4(),
            observation_op
        ));
        assert!(!delivery_observer_admitted(
            &observing,
            delivery_op,
            Uuid::new_v4()
        ));

        let backends = json!({"codex":{"status":"ok","skipped":0},
            "devin_acp":{"status":"ok","skipped":0}});
        #[cfg(feature = "network")]
        let backends = {
            let mut backends = backends;
            backends["opencode"] = json!({"status":"ok","skipped":0});
            backends["devin_cloud"] = json!({"status":"ok","skipped":0});
            backends
        };
        let idle = json!({"truncated":false,"backends":backends,
            "tasks":[{"task_id":"old","status":"completed"}]});
        assert!(initial_workspace_tasks_idle(&idle, &bound.task_id).is_ok());
        let mut active = idle.clone();
        active["tasks"][0]["status"] = json!("running");
        assert!(initial_workspace_tasks_idle(&active, &bound.task_id).is_err());
        let mut unavailable = idle.clone();
        unavailable["backends"]["devin_acp"]["status"] = json!("unavailable");
        assert!(initial_workspace_tasks_idle(&unavailable, &bound.task_id).is_err());
        let mut skipped = idle.clone();
        skipped["backends"]["codex"]["skipped"] = json!(1);
        assert!(initial_workspace_tasks_idle(&skipped, &bound.task_id).is_err());
        let mut truncated = idle;
        truncated["truncated"] = json!(true);
        assert!(initial_workspace_tasks_idle(&truncated, &bound.task_id).is_err());
    }

    #[test]
    fn binding_index_replays_exact_owner_and_refuses_reassignment() {
        let operation_id = Uuid::new_v4();
        let binding = ChangeBinding {
            owner_operation_id: Uuid::new_v4(),
            allocation_operation_id: operation_id,
            change_id: "change-a".to_owned(),
            task_id: Uuid::new_v4().to_string(),
            workspace_id: "workspace-a".to_owned(),
        };
        record_binding(&binding).unwrap();
        record_binding(&binding).unwrap();
        assert_eq!(read_binding(operation_id).unwrap(), Some(binding.clone()));
        let mut conflicting = binding;
        conflicting.change_id = "change-b".to_owned();
        assert!(record_binding(&conflicting).is_err());
    }

    #[test]
    fn snapshot_receipt_identity_is_stable_and_separates_generations() {
        let task = Uuid::new_v4();
        let first = boundary_snapshot_operation_id(task, "change", "execution-1", 2);
        assert_eq!(
            first,
            boundary_snapshot_operation_id(task, "change", "execution-1", 2)
        );
        assert_ne!(
            first,
            boundary_snapshot_operation_id(task, "change", "execution-2", 2)
        );
        assert_ne!(
            first,
            boundary_snapshot_operation_id(task, "change", "execution-1", 3)
        );
    }

    #[test]
    fn bound_resume_and_steer_wait_for_snapshot_receipt_and_initial_helper() {
        let (_temp, store, bound) = fixture_change();
        for action in ["resume", "steer"] {
            assert!(ensure_bound_control_snapshot_clear(action, &bound, true).is_err());
            assert!(ensure_bound_control_snapshot_clear(action, &bound, false).is_ok());
        }
        assert!(ensure_bound_control_snapshot_clear("interrupt", &bound, true).is_ok());

        let start = InitialStart {
            operation_id: Uuid::new_v4(),
            request_fingerprint: "request".into(),
            snapshot_operation_id: Uuid::new_v4(),
            execution_id: Uuid::new_v4().to_string(),
            snapshot_dispatch_attempted: false,
            snapshot_verified: false,
            vcs_operation_id: None,
        };
        let reserved = store
            .reserve_initial_start(&bound.change_id, bound.revision, start.clone())
            .unwrap();
        let attempted = store
            .mark_initial_snapshot_attempt(
                &bound.change_id,
                reserved.revision,
                start.snapshot_operation_id,
            )
            .unwrap();
        assert!(ensure_bound_control_snapshot_clear("resume", &attempted, false).is_err());
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
        assert!(ensure_bound_control_snapshot_clear("steer", &verified, false).is_ok());
    }

    #[test]
    fn typed_command_rejects_machine_mutation_parameters() {
        let operation_id = Uuid::new_v4();
        let valid = json!({"action":"create", "operation_id": operation_id, "task_id":"task", "parent_task_id":null, "parent_change_id":null, "base":{"kind":"origin_main"}});
        assert!(serde_json::from_value::<ChangeCommand>(valid.clone()).is_ok());
        for key in ["argv", "environment", "network_policy", "path", "merge"] {
            let mut invalid = valid.clone();
            invalid[key] = json!("unsafe");
            assert!(serde_json::from_value::<ChangeCommand>(invalid).is_err());
        }
        let invalid = json!({"action":"allocate", "change_id":"c", "expected_revision":1, "operation_id":"not-a-uuid"});
        assert!(serde_json::from_value::<ChangeCommand>(invalid).is_err());
    }

    #[test]
    fn cli_requires_bounded_typed_request_and_provisioning_identity() {
        let operation = Uuid::new_v4();
        let args = vec![
            "--provisioning-operation-id".to_owned(),
            operation.to_string(),
            "--request".to_owned(),
            json!({"action":"create_provisioned", "provisioning_operation_id":operation,
                "task_id":Uuid::new_v4(), "parent_task_id":null})
            .to_string(),
        ];
        assert!(matches!(
            parse(&args).unwrap().request,
            ChangeCommand::CreateProvisioned { .. }
        ));
        let mut oversized = args.clone();
        oversized[3] = "x".repeat(16 * 1024 + 1);
        assert!(parse(&oversized).is_err());
        let mut forged = args.clone();
        forged[3] = json!({"action":"reconcile_delivery", "change_ids":[],
            "change_id":"c", "expected_revision":1, "operation_id":operation,
            "model":"default", "effort":"medium",
            "pull_request":{"number":1}})
        .to_string();
        assert!(parse(&forged).is_err());
    }

    #[test]
    fn delegated_observation_report_has_closed_shape() {
        let mut report = json!({"operation_id":Uuid::new_v4(),
            "repository_id":"github.com/owner/repo", "change_id":"change",
            "observation":{"branch_revision":null,"pull_request":null}});
        assert!(serde_json::from_value::<RemoteObservationEnvelope>(report.clone()).is_ok());
        report["caller_receipt"] = json!("https://github.com/owner/repo/pull/1");
        assert!(serde_json::from_value::<RemoteObservationEnvelope>(report).is_err());
    }
}

#[cfg(test)]
mod admission_tests {
    #[test]
    fn competing_boundary_fails_until_original_owner_releases() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("admission.lock");
        let first = super::try_bound_admission(&path).unwrap();
        assert!(super::try_bound_admission(&path).is_err());
        drop(first);
        assert!(super::try_bound_admission(&path).is_ok());
    }
}
