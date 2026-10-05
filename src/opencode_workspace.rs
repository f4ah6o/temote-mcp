//! Pre-acceptance binding for OpenCode tasks that require a managed checkout.
//!
//! The caller obtains `ManagedWorkspace` from the host's provisioning receipt,
//! never from task JSON. This module does not grant shell access: OpenCode V2's
//! shell hook is mutable plugin state, not an enforced Temote sandbox boundary.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub(crate) struct ManagedWorkspace {
    pub workspace_id: Uuid,
    pub repository_id: String,
    pub checkout: PathBuf,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CapabilityBlocker {
    pub class: &'static str,
    pub backend: &'static str,
    pub workspace_id: Option<Uuid>,
    pub repository_id: Option<String>,
    pub path: PathBuf,
    pub missing_capability: &'static str,
    pub recovery_hint: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BoundWorkspace {
    pub workspace_id: Uuid,
    pub repository_id: String,
    pub cwd: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckAction {
    Inspect,
    Build,
    Test,
    Lint,
}

impl CheckAction {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Build => "build",
            Self::Test => "test",
            Self::Lint => "lint",
        }
    }

    pub(crate) fn task(self) -> &'static str {
        match self {
            Self::Inspect => {
                "Inspect the current managed checkout and its package scripts or standard build configuration. Do not execute an arbitrary command. Report the available checks and any blockers. Do not read protected agent, credential, or repository metadata state."
            }
            Self::Build => {
                "Run only the current checkout's standard package build script or standard build check through your existing sandbox and approval policy. Do not use caller-provided commands, paths, environment, or network policy. Report a concise result."
            }
            Self::Test => {
                "Run only the current checkout's standard package test script or standard test check through your existing sandbox and approval policy. Do not use caller-provided commands, paths, environment, or network policy. Report a concise result."
            }
            Self::Lint => {
                "Run only the current checkout's standard package lint script or standard lint check through your existing sandbox and approval policy. Do not use caller-provided commands, paths, environment, or network policy. Report a concise result."
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CheckRequest {
    pub action: CheckAction,
    pub operation_id: Uuid,
    pub status: bool,
}

/// Reject every field outside the narrow MCP schema even if a client skips
/// schema validation. `status` identifies the original fixed action for an
/// exact retained-receipt lookup.
pub(crate) fn parse_check_request(value: &Value) -> Option<CheckRequest> {
    let args = value.as_object()?;
    if !args
        .keys()
        .all(|key| matches!(key.as_str(), "action" | "operation_id" | "target"))
    {
        return None;
    }
    let verb = args.get("action")?.as_str()?;
    let status = verb == "status";
    let target = if status {
        args.get("target")?.as_str()?
    } else {
        if args.contains_key("target") {
            return None;
        }
        verb
    };
    let action = match target {
        "inspect" => CheckAction::Inspect,
        "build" => CheckAction::Build,
        "test" => CheckAction::Test,
        "lint" => CheckAction::Lint,
        _ => return None,
    };
    Some(CheckRequest {
        action,
        operation_id: Uuid::parse_str(args.get("operation_id")?.as_str()?).ok()?,
        status,
    })
}

pub(crate) fn blocker(
    class: &'static str,
    workspace: Option<&ManagedWorkspace>,
    path: &Path,
    missing_capability: &'static str,
    recovery_hint: &'static str,
) -> CapabilityBlocker {
    CapabilityBlocker {
        class,
        backend: "opencode",
        workspace_id: workspace.map(|item| item.workspace_id),
        repository_id: workspace.map(|item| item.repository_id.clone()),
        path: path.to_owned(),
        missing_capability,
        recovery_hint,
    }
}

/// Check that the active session names exactly the host-authorized checkout.
/// The session path and receipt path must already be canonical names; aliases
/// through symlinks are rejected even if they currently resolve to the same
/// directory. A later launcher must pin/revalidate this identity at use time.
pub(crate) fn bind(
    session_cwd: &Path,
    workspace: Option<&ManagedWorkspace>,
) -> Result<BoundWorkspace, Box<CapabilityBlocker>> {
    let Some(workspace) = workspace else {
        return Err(Box::new(blocker(
            "workspace_absent",
            None,
            session_cwd,
            "managed_workspace",
            "Start a managed session with a ready workspace before an implementation task.",
        )));
    };
    let expected = &workspace.checkout;
    let canonical_expected = fs::canonicalize(expected).ok();
    let canonical_session = fs::canonicalize(session_cwd).ok();
    if canonical_expected.as_deref() != Some(expected.as_path())
        || canonical_session.as_deref() != Some(session_cwd)
        || session_cwd != expected
        || !expected.is_dir()
    {
        return Err(Box::new(blocker(
            "workspace_mismatch",
            Some(workspace),
            session_cwd,
            "canonical_checkout_cwd",
            "Restore the managed checkout and start a session at its canonical path.",
        )));
    }
    let metadata_present = [".git", ".jj"].iter().any(|name| {
        fs::symlink_metadata(expected.join(name))
            .is_ok_and(|metadata| !metadata.file_type().is_symlink())
    });
    if !metadata_present {
        return Err(Box::new(blocker(
            "workspace_mismatch",
            Some(workspace),
            expected,
            "working_checkout",
            "Reconcile the managed workspace receipt and checkout before retrying.",
        )));
    }
    Ok(BoundWorkspace {
        workspace_id: workspace.workspace_id,
        repository_id: workspace.repository_id.clone(),
        cwd: expected.clone(),
    })
}

/// A managed checkout alone never authorizes the private command helper.
/// Host opt-in, Codex capability, and the live per-task MCP registration are
/// separate admission checks made by the OpenCode backend.
pub(crate) fn require_scoped_commands(bound: &BoundWorkspace) -> CapabilityBlocker {
    CapabilityBlocker {
        class: "execution_unavailable",
        backend: "opencode",
        workspace_id: Some(bound.workspace_id),
        repository_id: Some(bound.repository_id.clone()),
        path: bound.cwd.clone(),
        missing_capability: "scoped_command_execution",
        recovery_hint: "Configure the Host's private Codex model and effort opt-in; OpenCode shell remains denied.",
    }
}
