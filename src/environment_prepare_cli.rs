//! Typed local frontend for managed workspace environment preparation.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::environment_preparation::{
    Adapter, DelegatedExecution, PreparationPhase, PreparationPlan, PreparationReceipt,
};
use crate::{codex_app_server, config, observation, orchestration, repository_store};

pub(crate) const USAGE: &str = "Usage: temote env-prepare <start|status> --session-id <MANAGED_SESSION_ID> --operation-id <UUID> [--adapter <vp-pnpm|cargo-sccache> --model <MODEL> --effort <EFFORT>]\n";

pub(crate) enum Invocation {
    Start {
        session_id: String,
        operation_id: Uuid,
        adapter: Adapter,
        model: String,
        effort: String,
    },
    Status {
        session_id: String,
        operation_id: Uuid,
    },
}

pub(crate) fn parse(raw: &[String]) -> Result<Invocation, String> {
    parse_inner(raw).map_err(|_| format!("Invalid env-prepare request.\n{USAGE}"))
}

fn parse_inner(raw: &[String]) -> Result<Invocation> {
    let action = raw.first().context("env-prepare action is required")?;
    let mut session_id = None;
    let mut operation_id = None;
    let mut adapter = None;
    let mut model = None;
    let mut effort = None;
    let (args, remainder) = raw[1..].as_chunks::<2>();
    for pair in args {
        let slot = match pair[0].as_str() {
            "--session-id" => &mut session_id,
            "--operation-id" => &mut operation_id,
            "--adapter" => &mut adapter,
            "--model" => &mut model,
            "--effort" => &mut effort,
            _ => anyhow::bail!("unknown env-prepare option"),
        };
        ensure!(
            slot.replace(pair[1].clone()).is_none(),
            "duplicate env-prepare option"
        );
    }
    ensure!(remainder.is_empty(), "missing env-prepare option value");
    let session_id = session_id.context("--session-id is required")?;
    config::validate_session_id(&session_id)?;
    let operation_id = Uuid::parse_str(&operation_id.context("--operation-id is required")?)?;
    match action.as_str() {
        "start" => {
            let adapter = match adapter.as_deref() {
                Some("vp-pnpm") => Adapter::VpPnpm,
                Some("cargo-sccache") => Adapter::CargoSccache,
                _ => anyhow::bail!("--adapter must be vp-pnpm or cargo-sccache"),
            };
            let model = model.context("--model is required")?;
            let effort = effort.context("--effort is required")?;
            ensure!(
                !model.is_empty()
                    && !effort.is_empty()
                    && model.len() <= 256
                    && effort.len() <= 256,
                "invalid model or effort"
            );
            Ok(Invocation::Start {
                session_id,
                operation_id,
                adapter,
                model,
                effort,
            })
        }
        "status" => {
            ensure!(
                adapter.is_none() && model.is_none() && effort.is_none(),
                "status accepts only session and operation IDs"
            );
            Ok(Invocation::Status {
                session_id,
                operation_id,
            })
        }
        _ => anyhow::bail!("unknown env-prepare action"),
    }
}

pub(crate) async fn run(invocation: Invocation) -> Result<()> {
    let value = match invocation {
        Invocation::Start {
            session_id,
            operation_id,
            adapter,
            model,
            effort,
        } => start(&session_id, operation_id, adapter, &model, &effort).await?,
        Invocation::Status {
            session_id,
            operation_id,
        } => status(&session_id, operation_id).await?,
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn receipt_path(operation_id: Uuid) -> Result<PathBuf> {
    Ok(config::state_dir()?
        .join("environment-preparation")
        .join(format!("{operation_id}.json")))
}

fn read_receipt(path: &Path) -> Result<Option<PreparationReceipt>> {
    let parent = path.parent().context("preparation receipt has no parent")?;
    if !validate_store_directory(parent, false)? {
        return Ok(None);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("cannot safely open preparation receipt"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 16 * 1024,
        "preparation receipt is not a bounded regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o077 == 0,
            "preparation receipt is not private to this owner"
        );
    }
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16 * 1024, "preparation receipt is too large");
    let receipt: PreparationReceipt = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid preparation receipt"))?;
    ensure!(
        path.file_stem().and_then(|value| value.to_str())
            == Some(receipt.plan.operation_id.to_string().as_str())
            && receipt.plan.version == crate::environment_preparation::PREPARATION_VERSION,
        "preparation receipt identity is invalid"
    );
    Ok(Some(receipt))
}

fn validate_store_directory(path: &Path, create: bool) -> Result<bool> {
    let state = path
        .parent()
        .context("preparation store has no state directory")?;
    for directory in [state, path] {
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && directory == path && create =>
            {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(directory)?;
                fs::symlink_metadata(directory)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "preparation store directory is unsafe"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.permissions().mode() & 0o022 == 0,
                "preparation store directory is not owner controlled"
            );
        }
        ensure!(
            fs::canonicalize(directory)? == directory,
            "preparation store directory changed"
        );
    }
    Ok(true)
}

fn save_receipt(
    path: &Path,
    receipt: &PreparationReceipt,
    expected: Option<&PreparationReceipt>,
) -> Result<()> {
    let bytes = serde_json::to_vec(receipt)?;
    ensure!(bytes.len() <= 16 * 1024, "preparation receipt is too large");
    let parent = path.parent().context("preparation receipt has no parent")?;
    validate_store_directory(parent, true)?;
    ensure!(
        read_receipt(path)?.as_ref() == expected,
        "preparation receipt changed concurrently"
    );
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    let mut directory = OpenOptions::new();
    directory.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        directory.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    directory.open(parent)?.sync_all()?;
    Ok(())
}

async fn load_owner(
    session_id: &str,
) -> Result<(config::Session, repository_store::ProvisioningReceipt)> {
    ensure!(
        config::session_is_active(session_id).await?,
        "managed session is not active"
    );
    let session = config::read_session_metadata(session_id).await?;
    ensure!(
        !session.permission_mode.is_yolo(),
        "yolo session is not eligible"
    );
    let owner = repository_store::RepositoryStore::new()?
        .find_by_session_id(session_id)?
        .context("managed workspace owner receipt is missing")?;
    ensure!(
        owner.phase == repository_store::ProvisioningPhase::WorkspaceReady
            && owner
                .activated_owner
                .as_ref()
                .is_some_and(|instance| instance.matches(&session)),
        "managed workspace owner is not current"
    );
    ensure!(
        fs::canonicalize(&session.cwd)? == session.cwd,
        "managed workspace scope changed"
    );
    let roots = crate::named_roots::NamedRoots::from_env()?;
    let root = roots
        .canonical_root(&owner.root_name)
        .context("managed named root is unavailable")?;
    ensure!(
        owner.canonical_root.as_deref() == Some(root)
            && root.join(crate::workspace_provisioning::workspace_relative(&owner)) == session.cwd,
        "managed workspace allocation changed"
    );
    ensure!(
        crate::workspace_provisioning::inspect_ready(root, &owner)?
            == owner.pinned_base.as_deref().unwrap_or_default(),
        "managed allocation revision changed"
    );
    Ok((session, owner))
}

fn view(receipt: &PreparationReceipt) -> Value {
    json!({"schema_version": 1, "session_id": receipt.plan.session.session_id,
        "workspace_id": receipt.plan.workspace_id, "operation_id": receipt.plan.operation_id,
        "phase": receipt.phase, "task_id": receipt.task_id,
        "retryable": receipt.phase == PreparationPhase::Failed,
        "reconciliation_required": receipt.phase == PreparationPhase::Uncertain})
}

async fn start(
    session_id: &str,
    operation_id: Uuid,
    adapter: Adapter,
    model: &str,
    effort: &str,
) -> Result<Value> {
    let (session, owner) = load_owner(session_id).await?;
    let plan = PreparationPlan::new(
        &session,
        owner.workspace_id,
        operation_id,
        owner.request.repository.clone(),
        &session.cwd,
        adapter,
    )?;
    let prompt = plan.delegated_task()?;
    let path = receipt_path(operation_id)?;
    let existing = {
        let _lock = config::acquire_session_lifecycle_lock().await?;
        if let Some(existing) = read_receipt(&path)? {
            ensure!(
                existing.plan == plan
                    && existing.model.as_deref() == Some(model)
                    && existing.effort.as_deref() == Some(effort),
                "operation_conflict: preparation inputs or model selection changed"
            );
            true
        } else {
            let mut receipt = PreparationReceipt::new(plan.clone());
            receipt.model = Some(model.to_owned());
            receipt.effort = Some(effort.to_owned());
            receipt.begin()?;
            save_receipt(&path, &receipt, None)?;
            false
        }
    };
    if existing {
        return status(session_id, operation_id).await;
    }
    let args = json!({"session_id": session.id, "operation_id": operation_id,
        "task": prompt, "model": model, "effort": effort});
    let actor = observation::ActorRef {
        transport: "cli".into(),
        principal: None,
    };
    let result = orchestration::invoke_codex_task_start_with_admission(
        &args,
        &session,
        &codex_app_server::TaskStartOrigin::Generic,
        &actor,
        None,
        || {
            plan.validate_scope()?;
            let current = repository_store::RepositoryStore::new()?
                .find_by_session_id(session_id)?
                .context("managed owner disappeared")?;
            ensure!(
                current.operation_id == owner.operation_id
                    && current
                        .activated_owner
                        .as_ref()
                        .is_some_and(|instance| instance.matches(&session)),
                "managed owner changed before delegation"
            );
            ensure!(
                fs::canonicalize(&session.cwd)? == session.cwd,
                "session scope changed"
            );
            Ok(())
        },
    )
    .await;
    match result {
        Ok(task) => {
            let task_id = task
                .get("task_id")
                .and_then(Value::as_str)
                .context("delegated preparation task_id is missing")?;
            let _lock = config::acquire_session_lifecycle_lock().await?;
            let (current_session, current_owner) = load_owner(session_id).await?;
            ensure!(
                plan.session.matches(&current_session)
                    && current_owner.operation_id == owner.operation_id,
                "managed session changed before task receipt update"
            );
            let mut receipt = read_receipt(&path)?.context("preparation receipt disappeared")?;
            let previous = receipt.clone();
            receipt.record_task(Uuid::parse_str(task_id)?)?;
            save_receipt(&path, &receipt, Some(&previous))?;
            Ok(view(&receipt))
        }
        Err(error) => {
            // The start may have crossed the backend boundary. Preserve the
            // attempted receipt for task reconciliation; never replay blindly.
            Err(error.context("preparation start outcome uncertain; inspect retained task with the same operation ID"))
        }
    }
}

async fn status(session_id: &str, operation_id: Uuid) -> Result<Value> {
    let (session, owner) = load_owner(session_id).await?;
    let path = receipt_path(operation_id)?;
    let mut receipt = read_receipt(&path)?.context("preparation receipt is missing")?;
    ensure!(
        receipt.plan.session.matches(&session)
            && receipt.plan.workspace_id == owner.workspace_id
            && receipt.plan.repository_id == owner.request.repository,
        "preparation receipt does not match current managed owner"
    );
    if receipt.task_id.is_none() {
        let model = receipt
            .model
            .as_deref()
            .context("retained preparation model is missing")?;
        let effort = receipt
            .effort
            .as_deref()
            .context("retained preparation effort is missing")?;
        let replay = codex_app_server::task_start_replay_if_retained(
            &json!({"session_id": session.id, "operation_id": receipt.plan.operation_id,
                "task": receipt.plan.delegated_task()?, "model": model, "effort": effort}),
            &session,
            &codex_app_server::TaskStartOrigin::Generic,
        )?;
        let recovered = replay
            .as_ref()
            .and_then(|value| value.get("task_id"))
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok());
        let _lock = config::acquire_session_lifecycle_lock().await?;
        let (current_session, current_owner) = load_owner(session_id).await?;
        ensure!(
            receipt.plan.session.matches(&current_session)
                && current_owner.operation_id == owner.operation_id,
            "managed owner changed during reconciliation"
        );
        let previous = read_receipt(&path)?.context("preparation receipt disappeared")?;
        ensure!(
            previous == receipt,
            "preparation receipt changed concurrently"
        );
        if let Some(task_id) = recovered {
            receipt.record_task(task_id)?;
        } else {
            receipt.reconcile(&session, DelegatedExecution::Unknown)?;
        }
        save_receipt(&path, &receipt, Some(&previous))?;
        if recovered.is_none() {
            return Ok(view(&receipt));
        }
    }
    let task_id = receipt.task_id.context("retained task identity missing")?;
    let actor = observation::ActorRef {
        transport: "cli".into(),
        principal: None,
    };
    let task = orchestration::invoke(
        orchestration::Backend::Codex,
        orchestration::Operation::TaskGet,
        &json!({"session_id": session.id, "task_id": task_id}),
        &session,
        &actor,
        None,
    )
    .await;
    let execution = match task
        .as_ref()
        .ok()
        .and_then(|task| task.get("status"))
        .and_then(Value::as_str)
    {
        Some("completed") => DelegatedExecution::Completed,
        Some("failed" | "blocked") => DelegatedExecution::Failed,
        Some("running" | "pending" | "waiting_approval" | "waiting_input") => {
            DelegatedExecution::Running
        }
        _ => DelegatedExecution::Unknown,
    };
    let previous = receipt.clone();
    receipt.reconcile(&session, execution)?;
    let _lock = config::acquire_session_lifecycle_lock().await?;
    let (current_session, current_owner) = load_owner(session_id).await?;
    ensure!(
        receipt.plan.session.matches(&current_session)
            && current_owner.operation_id == owner.operation_id,
        "managed owner changed during reconciliation"
    );
    save_receipt(&path, &receipt, Some(&previous))?;
    Ok(view(&receipt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_only_typed_options() {
        let words = |items: &[&str]| {
            items
                .iter()
                .map(|item| item.to_string())
                .collect::<Vec<_>>()
        };
        let id = "00000000-0000-0000-0000-000000000001";
        assert!(
            parse_inner(&words(&[
                "start",
                "--session-id",
                "s",
                "--operation-id",
                id,
                "--adapter",
                "vp-pnpm",
                "--model",
                "m",
                "--effort",
                "medium"
            ]))
            .is_ok()
        );
        assert!(
            parse_inner(&words(&[
                "start",
                "--session-id",
                "s",
                "--operation-id",
                id,
                "--argv",
                "sh"
            ]))
            .is_err()
        );
        assert!(
            parse_inner(&words(&[
                "status",
                "--session-id",
                "s",
                "--operation-id",
                id,
                "--adapter",
                "vp-pnpm"
            ]))
            .is_err()
        );
    }
}
