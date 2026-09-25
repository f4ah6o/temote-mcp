//! Transport-independent entry point for server-backed coding-agent task
//! operations.
//!
//! Approval decisions (operation class x permission mode), approval
//! detail/metadata payloads, and dispatch onto the backends live here so the
//! MCP frontend, the local control plane, and future frontends share one
//! execution path. Frontends normalize their request onto a [`Backend`] +
//! [`Operation`] and call [`invoke`]; they must not call backend modules or
//! run approvals on their own.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::Value;

use temote_mcp::activity::contract::{ActivityErrorKind, ActivitySummary};
use temote_mcp::activity::scope::ActivityScope;

use crate::{approvals, codex_app_server, config, devin_acp};
#[cfg(feature = "network")]
use crate::{devin_cloud, opencode_server};

/// One server-backed coding agent behind the shared typed task contract.
///
/// Backend-specific options and capability differences (hosted execution,
/// resume support, the meaning of interrupt) stay inside each backend
/// module; this enum only names which backend owns an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Backend {
    Codex,
    #[cfg(feature = "network")]
    OpenCode,
    DevinAcp,
    #[cfg(feature = "network")]
    DevinCloud,
}

/// One operation of the shared typed task contract. `Status` is a read-only
/// compatibility probe; the task operations accept, read, or control
/// retained tasks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    Status,
    TaskStart,
    TaskGet,
    TaskControl,
}

impl Backend {
    /// Compatibility operation label shared by the public tool surface and
    /// the approval layer (e.g. `codex_task_start`). Approval requests and
    /// metadata keep using these labels so records stay comparable across
    /// transports.
    fn operation_name(self, operation: Operation) -> String {
        let prefix = match self {
            Backend::Codex => "codex",
            #[cfg(feature = "network")]
            Backend::OpenCode => "opencode",
            Backend::DevinAcp => "devin",
            #[cfg(feature = "network")]
            Backend::DevinCloud => "devin_cloud",
        };
        let action = match operation {
            Operation::Status => "status",
            Operation::TaskStart => "task_start",
            Operation::TaskGet => "task_get",
            Operation::TaskControl => "task_control",
        };
        format!("{prefix}_{action}")
    }

    /// The approval policy class this backend's operations belong to.
    fn approval_class(self) -> approvals::ApprovalClass {
        match self {
            Backend::Codex => approvals::ApprovalClass::CodexAppServer,
            #[cfg(feature = "network")]
            Backend::OpenCode => approvals::ApprovalClass::OpenCodeServer,
            Backend::DevinAcp => approvals::ApprovalClass::DevinAcp,
            #[cfg(feature = "network")]
            Backend::DevinCloud => approvals::ApprovalClass::DevinCloud,
        }
    }

    /// Delegation provenance recorded in approval metadata.
    fn provenance(self) -> &'static str {
        match self {
            Backend::Codex => "codex_delegation",
            #[cfg(feature = "network")]
            Backend::OpenCode => "opencode_delegation",
            Backend::DevinAcp => "devin_delegation",
            #[cfg(feature = "network")]
            Backend::DevinCloud => "devin_cloud_delegation",
        }
    }

    /// Scope label recorded in approval metadata.
    fn approval_scope(self) -> &'static str {
        match self {
            Backend::Codex | Backend::DevinAcp => "session_cwd",
            #[cfg(feature = "network")]
            Backend::OpenCode => "session_cwd",
            #[cfg(feature = "network")]
            Backend::DevinCloud => "devin_cloud",
        }
    }

    /// Error returned when the user denies an operation for this backend.
    fn denial(self) -> &'static str {
        match self {
            Backend::Codex => "user denied Codex operation",
            #[cfg(feature = "network")]
            Backend::OpenCode => "user denied OpenCode operation",
            Backend::DevinAcp => "user denied Devin operation",
            #[cfg(feature = "network")]
            Backend::DevinCloud => "user denied Devin Cloud operation",
        }
    }

    async fn status(self, session: &config::Session) -> Result<Value> {
        match self {
            Backend::Codex => codex_app_server::status(session).await,
            #[cfg(feature = "network")]
            Backend::OpenCode => opencode_server::status(session).await,
            Backend::DevinAcp => devin_acp::status(session).await,
            #[cfg(feature = "network")]
            Backend::DevinCloud => devin_cloud::status(session).await,
        }
    }

    async fn task_start(self, args: &Value, session: &config::Session) -> Result<Value> {
        match self {
            Backend::Codex => codex_app_server::task_start(args, session).await,
            #[cfg(feature = "network")]
            Backend::OpenCode => opencode_server::task_start(args, session).await,
            Backend::DevinAcp => devin_acp::task_start(args, session).await,
            #[cfg(feature = "network")]
            Backend::DevinCloud => devin_cloud::task_start(args, session).await,
        }
    }

    async fn task_get(self, args: &Value, session: &config::Session) -> Result<Value> {
        match self {
            Backend::Codex => codex_app_server::task_get(args, session).await,
            #[cfg(feature = "network")]
            Backend::OpenCode => opencode_server::task_get(args, session).await,
            Backend::DevinAcp => devin_acp::task_get(args, session).await,
            #[cfg(feature = "network")]
            Backend::DevinCloud => devin_cloud::task_get(args, session).await,
        }
    }

    async fn task_control(self, args: &Value, session: &config::Session) -> Result<Value> {
        match self {
            Backend::Codex => codex_app_server::task_control(args, session).await,
            #[cfg(feature = "network")]
            Backend::OpenCode => opencode_server::task_control(args, session).await,
            Backend::DevinAcp => devin_acp::task_control(args, session).await,
            #[cfg(feature = "network")]
            Backend::DevinCloud => devin_cloud::task_control(args, session).await,
        }
    }
}

/// Run one typed operation through the shared approval and dispatch entry.
///
/// `args` is the operation's typed input object; `activity` is the optional
/// caller-owned activity scope the frontend already opened. Approval runs
/// before any backend side effect; `TaskGet` is a read and reconciles
/// without an approval prompt, matching the existing contract.
pub(crate) async fn invoke(
    backend: Backend,
    operation: Operation,
    args: &Value,
    session: &config::Session,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    match operation {
        Operation::Status => {
            let (detail, metadata) = status_approval(backend);
            authorize(backend, operation, session, detail, metadata, activity).await?;
            backend.status(session).await
        }
        Operation::TaskStart => {
            let (detail, metadata) = task_start_approval(backend, args);
            authorize(backend, operation, session, detail, metadata, activity).await?;
            backend.task_start(args, session).await
        }
        Operation::TaskGet => backend.task_get(args, session).await,
        Operation::TaskControl => {
            let (detail, metadata) = task_control_approval(backend, args);
            authorize(backend, operation, session, detail, metadata, activity).await?;
            backend.task_control(args, session).await
        }
    }
}

async fn authorize(
    backend: Backend,
    operation: Operation,
    session: &config::Session,
    detail: String,
    metadata: BTreeMap<String, String>,
    activity: Option<&ActivityScope>,
) -> Result<()> {
    let operation_name = backend.operation_name(operation);
    let approved = approvals::ensure_local_approval_with_activity(
        session,
        backend.approval_class(),
        &operation_name,
        detail,
        session.cwd.clone(),
        metadata,
        activity,
    )
    .await?;
    finish_activity_approval(approved, activity, backend.denial())
}

pub(crate) fn finish_activity_approval(
    approved: bool,
    activity: Option<&ActivityScope>,
    denial: &'static str,
) -> Result<()> {
    if !approved {
        if let Some(activity) = activity {
            let _ = activity
                .fail_with_summary(ActivitySummary::failure(ActivityErrorKind::ApprovalDenied));
        }
        anyhow::bail!(denial);
    }
    if let Some(activity) = activity {
        let _ = activity.running();
    }
    Ok(())
}

fn status_approval(backend: Backend) -> (String, BTreeMap<String, String>) {
    match backend {
        Backend::Codex => codex_status_approval(),
        #[cfg(feature = "network")]
        Backend::OpenCode => opencode_status_approval(),
        Backend::DevinAcp => devin_status_approval(),
        #[cfg(feature = "network")]
        Backend::DevinCloud => devin_cloud_status_approval(),
    }
}

fn task_start_approval(backend: Backend, args: &Value) -> (String, BTreeMap<String, String>) {
    match backend {
        Backend::Codex => codex_task_start_approval(args),
        #[cfg(feature = "network")]
        Backend::OpenCode => opencode_task_start_approval(args),
        Backend::DevinAcp => devin_task_start_approval(args),
        #[cfg(feature = "network")]
        Backend::DevinCloud => devin_cloud_task_start_approval(args),
    }
}

fn task_control_approval(backend: Backend, args: &Value) -> (String, BTreeMap<String, String>) {
    match backend {
        Backend::Codex => codex_task_control_approval(args),
        #[cfg(feature = "network")]
        Backend::OpenCode => opencode_task_control_approval(args),
        Backend::DevinAcp => devin_task_control_approval(args),
        #[cfg(feature = "network")]
        Backend::DevinCloud => devin_cloud_task_control_approval(args),
    }
}

fn codex_status_approval() -> (String, BTreeMap<String, String>) {
    (
        "Codex delegation request\naccess: read-only\nscope: current session\nresult: model and effort compatibility metadata".to_owned(),
        approval_metadata(Backend::Codex, "codex_status", "status", false, "session_scope"),
    )
}

fn codex_task_start_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let operation_id = safe_approval_argument(args, "operation_id");
    let model = safe_approval_argument(args, "model");
    let effort = safe_approval_argument(args, "effort");
    let mut metadata = approval_metadata(
        Backend::Codex,
        "codex_task_start",
        "task_start",
        true,
        "session_scope",
    );
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("model".to_owned(), model.clone());
    metadata.insert("effort".to_owned(), effort.clone());
    metadata.insert("task_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Codex delegation request\noperation: start task\nmutation: workspace-write\nscope: current session working directory\nmodel: {model}\neffort: {effort}\noperation_id: {operation_id}\ntask input: omitted"
        ),
        metadata,
    )
}

fn codex_task_control_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let task_id = safe_approval_argument(args, "task_id");
    let operation_id = safe_approval_argument(args, "operation_id");
    let action = safe_approval_argument(args, "action");
    let mut metadata = approval_metadata(
        Backend::Codex,
        "codex_task_control",
        "task_control",
        true,
        &format!("task:{task_id}"),
    );
    metadata.insert("task_id".to_owned(), task_id.clone());
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("action".to_owned(), action.clone());
    metadata.insert("control_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Codex delegation request\noperation: control task\naction: {action}\nmutation: task control\ntarget: task {task_id}\nscope: current session working directory\noperation_id: {operation_id}\ncontrol input: omitted"
        ),
        metadata,
    )
}

#[cfg(feature = "network")]
fn opencode_status_approval() -> (String, BTreeMap<String, String>) {
    (
        "OpenCode delegation request\naccess: read-only\nscope: current session\nresult: serve compatibility and provider metadata".to_owned(),
        approval_metadata(Backend::OpenCode, "opencode_status", "status", false, "session_scope"),
    )
}

#[cfg(feature = "network")]
fn opencode_task_start_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let operation_id = safe_approval_argument(args, "operation_id");
    let model = safe_approval_argument(args, "model");
    let agent = safe_approval_argument(args, "agent");
    let variant = safe_approval_argument(args, "variant");
    let mut metadata = approval_metadata(
        Backend::OpenCode,
        "opencode_task_start",
        "task_start",
        true,
        "session_scope",
    );
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("model".to_owned(), model.clone());
    metadata.insert("agent".to_owned(), agent.clone());
    metadata.insert("variant".to_owned(), variant.clone());
    metadata.insert("task_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "OpenCode delegation request\noperation: start task\nmutation: workspace-write\nscope: current session working directory\nmodel: {model}\nagent: {agent}\nvariant: {variant}\noperation_id: {operation_id}\ntask input: omitted"
        ),
        metadata,
    )
}

#[cfg(feature = "network")]
fn opencode_task_control_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let task_id = safe_approval_argument(args, "task_id");
    let operation_id = safe_approval_argument(args, "operation_id");
    let action = safe_approval_argument(args, "action");
    let mut metadata = approval_metadata(
        Backend::OpenCode,
        "opencode_task_control",
        "task_control",
        true,
        &format!("task:{task_id}"),
    );
    metadata.insert("task_id".to_owned(), task_id.clone());
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("action".to_owned(), action.clone());
    metadata.insert("control_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "OpenCode delegation request\noperation: control task\naction: {action}\nmutation: task control\ntarget: task {task_id}\nscope: current session working directory\noperation_id: {operation_id}\ncontrol input: omitted"
        ),
        metadata,
    )
}

fn devin_status_approval() -> (String, BTreeMap<String, String>) {
    (
        "Devin delegation request\naccess: read-only\nscope: current session\nresult: acp capability and agent metadata".to_owned(),
        approval_metadata(Backend::DevinAcp, "devin_status", "status", false, "session_scope"),
    )
}

fn devin_task_start_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let operation_id = safe_approval_argument(args, "operation_id");
    let model = safe_approval_argument(args, "model");
    let agent = safe_approval_argument(args, "agent");
    let cloud = args.get("cloud").and_then(Value::as_bool).unwrap_or(false);
    let mut metadata = approval_metadata(
        Backend::DevinAcp,
        "devin_task_start",
        "task_start",
        true,
        "session_scope",
    );
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("model".to_owned(), model.clone());
    metadata.insert("agent".to_owned(), agent.clone());
    metadata.insert("cloud".to_owned(), cloud.to_string());
    metadata.insert("task_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Devin delegation request\noperation: start task\nmutation: workspace-write\nscope: current session working directory\nmodel: {model}\nagent: {agent}\ncloud: {cloud}\noperation_id: {operation_id}\ntask input: omitted"
        ),
        metadata,
    )
}

fn devin_task_control_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let task_id = safe_approval_argument(args, "task_id");
    let operation_id = safe_approval_argument(args, "operation_id");
    let action = safe_approval_argument(args, "action");
    let mut metadata = approval_metadata(
        Backend::DevinAcp,
        "devin_task_control",
        "task_control",
        true,
        &format!("task:{task_id}"),
    );
    metadata.insert("task_id".to_owned(), task_id.clone());
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("action".to_owned(), action.clone());
    metadata.insert("control_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Devin delegation request\noperation: control task\naction: {action}\nmutation: task control\ntarget: task {task_id}\nscope: current session working directory\noperation_id: {operation_id}\ncontrol input: omitted"
        ),
        metadata,
    )
}

#[cfg(feature = "network")]
fn devin_cloud_status_approval() -> (String, BTreeMap<String, String>) {
    (
        "Devin Cloud delegation request\naccess: read-only\nscope: current session\nresult: authenticated principal and organization (credential value omitted)".to_owned(),
        approval_metadata(
            Backend::DevinCloud,
            "devin_cloud_status",
            "status",
            false,
            "session_scope",
        ),
    )
}

#[cfg(feature = "network")]
fn devin_cloud_task_start_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let operation_id = safe_approval_argument(args, "operation_id");
    let title = safe_approval_argument(args, "title");
    let devin_mode = safe_approval_argument(args, "devin_mode");
    let repos = args
        .get("repos")
        .and_then(Value::as_array)
        .map(|items| items.len().to_string())
        .unwrap_or_else(|| "0".to_owned());
    let mut metadata = approval_metadata(
        Backend::DevinCloud,
        "devin_cloud_task_start",
        "task_start",
        true,
        "devin_cloud_session",
    );
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("title".to_owned(), title.clone());
    metadata.insert("devin_mode".to_owned(), devin_mode.clone());
    metadata.insert("repos".to_owned(), repos.clone());
    metadata.insert("task_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Devin Cloud delegation request\noperation: start hosted session\nmutation: remote Devin Cloud session (consumes ACUs)\nscope: Devin Cloud organization, not this host\ntitle: {title}\ndevin_mode: {devin_mode}\nrepos: {repos}\noperation_id: {operation_id}\ntask input: omitted"
        ),
        metadata,
    )
}

#[cfg(feature = "network")]
fn devin_cloud_task_control_approval(args: &Value) -> (String, BTreeMap<String, String>) {
    let task_id = safe_approval_argument(args, "task_id");
    let operation_id = safe_approval_argument(args, "operation_id");
    let action = safe_approval_argument(args, "action");
    let mut metadata = approval_metadata(
        Backend::DevinCloud,
        "devin_cloud_task_control",
        "task_control",
        true,
        &format!("task:{task_id}"),
    );
    metadata.insert("task_id".to_owned(), task_id.clone());
    metadata.insert("operation_id".to_owned(), operation_id.clone());
    metadata.insert("action".to_owned(), action.clone());
    metadata.insert("control_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Devin Cloud delegation request\noperation: control hosted session\naction: {action}\nmutation: remote Devin Cloud session\ntarget: task {task_id}\noperation_id: {operation_id}\ncontrol input: omitted"
        ),
        metadata,
    )
}

fn approval_metadata(
    backend: Backend,
    tool: &str,
    operation_type: &str,
    mutation: bool,
    target: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("provenance".to_owned(), backend.provenance().to_owned()),
        ("source".to_owned(), backend.provenance().to_owned()),
        ("tool".to_owned(), tool.to_owned()),
        ("operation_type".to_owned(), operation_type.to_owned()),
        ("target".to_owned(), target.to_owned()),
        ("mutation".to_owned(), mutation.to_string()),
        ("read_only".to_owned(), (!mutation).to_string()),
        ("scope".to_owned(), backend.approval_scope().to_owned()),
    ])
}

fn safe_approval_argument(args: &Value, key: &str) -> String {
    let Some(value) = args.get(key).and_then(Value::as_str) else {
        return "(not provided)".to_owned();
    };
    let mut rendered = String::new();
    for character in value.chars() {
        let part = if character.is_control() {
            if character.is_ascii() {
                format!("\\x{:02x}", character as u32)
            } else {
                format!("\\u{{{:x}}}", character as u32)
            }
        } else {
            character.to_string()
        };
        if rendered.len().saturating_add(part.len()) > 256 {
            rendered.push('…');
            break;
        }
        rendered.push_str(&part);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn codex_approval_details_are_actionable_without_task_input() {
        let task_marker = "prompt-secret-marker";
        let (start_detail, start_metadata) = codex_task_start_approval(&json!({
            "operation_id": "0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa",
            "task": task_marker,
            "model": "gpt-5.6-luna",
            "effort": "max"
        }));
        assert!(start_detail.contains("operation: start task"));
        assert!(start_detail.contains("model: gpt-5.6-luna"));
        assert!(start_detail.contains("effort: max"));
        assert!(start_detail.contains("task input: omitted"));
        assert!(!start_detail.contains(task_marker));
        assert_eq!(start_metadata["provenance"], "codex_delegation");
        assert_eq!(start_metadata["tool"], "codex_task_start");
        assert_eq!(start_metadata["mutation"], "true");
        assert_eq!(start_metadata["task_input"], "omitted");
        assert!(
            !serde_json::to_string(&start_metadata)
                .unwrap()
                .contains(task_marker)
        );

        let (control_detail, control_metadata) = codex_task_control_approval(&json!({
            "task_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb",
            "operation_id": "0199cccc-cccc-7ccc-8ccc-cccccccccccc",
            "action": "steer",
            "input": task_marker
        }));
        assert!(control_detail.contains("action: steer"));
        assert!(control_detail.contains("target: task 0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb"));
        assert!(control_detail.contains("control input: omitted"));
        assert!(!control_detail.contains(task_marker));
        assert_eq!(control_metadata["operation_type"], "task_control");
        assert_eq!(
            control_metadata["task_id"],
            "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb"
        );
        assert_eq!(control_metadata["control_input"], "omitted");

        let (_, status_metadata) = codex_status_approval();
        assert_eq!(status_metadata["read_only"], "true");
        assert_eq!(status_metadata["mutation"], "false");
    }

    #[test]
    fn operation_name_recovers_the_compatibility_tool_names() {
        let cases = [
            (Backend::Codex, Operation::Status, "codex_status"),
            (Backend::Codex, Operation::TaskStart, "codex_task_start"),
            (Backend::Codex, Operation::TaskGet, "codex_task_get"),
            (Backend::Codex, Operation::TaskControl, "codex_task_control"),
            #[cfg(feature = "network")]
            (Backend::OpenCode, Operation::Status, "opencode_status"),
            #[cfg(feature = "network")]
            (
                Backend::OpenCode,
                Operation::TaskStart,
                "opencode_task_start",
            ),
            #[cfg(feature = "network")]
            (Backend::OpenCode, Operation::TaskGet, "opencode_task_get"),
            #[cfg(feature = "network")]
            (
                Backend::OpenCode,
                Operation::TaskControl,
                "opencode_task_control",
            ),
            (Backend::DevinAcp, Operation::Status, "devin_status"),
            (Backend::DevinAcp, Operation::TaskStart, "devin_task_start"),
            (Backend::DevinAcp, Operation::TaskGet, "devin_task_get"),
            (
                Backend::DevinAcp,
                Operation::TaskControl,
                "devin_task_control",
            ),
            #[cfg(feature = "network")]
            (Backend::DevinCloud, Operation::Status, "devin_cloud_status"),
            #[cfg(feature = "network")]
            (
                Backend::DevinCloud,
                Operation::TaskStart,
                "devin_cloud_task_start",
            ),
            #[cfg(feature = "network")]
            (
                Backend::DevinCloud,
                Operation::TaskGet,
                "devin_cloud_task_get",
            ),
            #[cfg(feature = "network")]
            (
                Backend::DevinCloud,
                Operation::TaskControl,
                "devin_cloud_task_control",
            ),
        ];
        for (backend, operation, expected) in cases {
            assert_eq!(backend.operation_name(operation), expected);
        }
    }

    #[test]
    fn approval_policy_keeps_every_backend_on_the_shared_seam() {
        use approvals::LocalApproval;
        use config::PermissionMode;

        let backends = [
            Backend::Codex,
            #[cfg(feature = "network")]
            Backend::OpenCode,
            Backend::DevinAcp,
            #[cfg(feature = "network")]
            Backend::DevinCloud,
        ];
        for backend in backends {
            let class = backend.approval_class();
            // agent mode: a permitted start dispatches without a prompt.
            assert_eq!(
                approvals::local_approval(PermissionMode::Agent, class),
                LocalApproval::Skip
            );
            assert_eq!(
                approvals::local_approval(PermissionMode::Yolo, class),
                LocalApproval::Skip
            );
            // ask mode keeps the existing approval path.
            assert_eq!(
                approvals::local_approval(PermissionMode::Ask, class),
                LocalApproval::Request
            );
            // denial bails before dispatch (backend side effects: zero).
            let denied = finish_activity_approval(false, None, backend.denial());
            assert_eq!(denied.unwrap_err().to_string(), backend.denial());
        }
    }
}
