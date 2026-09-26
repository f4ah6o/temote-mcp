//! Transport-independent entry point for server-backed coding-agent task
//! operations.
//!
//! Approval decisions (operation class x permission mode), approval
//! detail/metadata payloads, and dispatch onto the backends live here so the
//! MCP frontend, the local control plane, and future frontends share one
//! execution path. Frontends normalize their request onto a [`Backend`] +
//! [`Operation`] and call [`invoke`]; they must not call backend modules or
//! run approvals on their own.
//!
//! [`TaskRequest`] is the typed request boundary in front of [`invoke`]:
//! raw input is validated onto it (including backend-specific options and
//! unsupported actions) before any approval prompt or backend side effect.

mod requests;

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use temote_mcp::activity::contract::{ActivityErrorKind, ActivitySummary};
use temote_mcp::activity::scope::ActivityScope;

use crate::{approvals, codex_app_server, config, devin_acp, observation};
#[cfg(feature = "network")]
use crate::{devin_cloud, opencode_server};

use requests::{
    CodexStartOptions, DevinAcpStartOptions, ExecutionLocality, TaskControlRequest,
    TaskStartRequest,
};
pub(crate) use requests::{ControlAction, TaskRequest};
#[cfg(feature = "network")]
use requests::{DevinCloudStartOptions, OpenCodeStartOptions};

/// One server-backed coding agent behind the shared typed task contract.
///
/// Capability differences (hosted execution, resume support, the meaning of
/// interrupt, wait-for-input) are typed data on the request boundary in
/// [`requests`]; this enum only names which backend owns an operation.
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
    /// Read-only projection of the caller's retained tasks. Local
    /// protocol packets surface it as a wire operation; today only
    /// tests and [`task_list`] reach it.
    #[allow(dead_code)]
    TaskList,
    TaskControl,
}

impl Backend {
    /// Compatibility operation label shared by the public tool surface and
    /// the approval layer (e.g. `codex_task_start`). Approval requests and
    /// metadata keep using these labels so records stay comparable across
    /// transports.
    pub(crate) fn operation_name(self, operation: Operation) -> String {
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
            Operation::TaskList => "task_list",
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

    /// Scope label recorded in approval metadata, resolved from the
    /// backend's static execution locality.
    fn approval_scope(self) -> &'static str {
        match self.capabilities().execution {
            ExecutionLocality::HostLocal => "session_cwd",
            ExecutionLocality::Hosted => "devin_cloud",
        }
    }

    /// Stable lowercase identity used as the merge key in the common
    /// [`task_list`] projection (`backends` map, item `backend` field) and
    /// as the backend identity in observation/journal records.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Backend::Codex => "codex",
            #[cfg(feature = "network")]
            Backend::OpenCode => "opencode",
            Backend::DevinAcp => "devin_acp",
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
        #[cfg(test)]
        tests::note_backend_dispatch();
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
        #[cfg(test)]
        tests::note_backend_dispatch();
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
        #[cfg(test)]
        tests::note_backend_dispatch();
        match self {
            Backend::Codex => codex_app_server::task_get(args, session).await,
            #[cfg(feature = "network")]
            Backend::OpenCode => opencode_server::task_get(args, session).await,
            Backend::DevinAcp => devin_acp::task_get(args, session).await,
            #[cfg(feature = "network")]
            Backend::DevinCloud => devin_cloud::task_get(args, session).await,
        }
    }

    async fn task_list(self, args: &Value, session: &config::Session) -> Result<Value> {
        #[cfg(test)]
        tests::note_backend_dispatch();
        match self {
            Backend::Codex => codex_app_server::task_list(args, session).await,
            #[cfg(feature = "network")]
            Backend::OpenCode => opencode_server::task_list(args, session).await,
            Backend::DevinAcp => devin_acp::task_list(args, session).await,
            #[cfg(feature = "network")]
            Backend::DevinCloud => devin_cloud::task_list(args, session).await,
        }
    }

    async fn task_control(self, args: &Value, session: &config::Session) -> Result<Value> {
        #[cfg(test)]
        tests::note_backend_dispatch();
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
///
/// `actor` is the caller's transport identity for the observation journal:
/// every normalized instruction and its caller-visible outcome are recorded
/// here once, so the same semantic operation observes identically across
/// frontends. Recording is best-effort and never fails the operation.
pub(crate) async fn invoke(
    backend: Backend,
    operation: Operation,
    args: &Value,
    session: &config::Session,
    actor: &observation::ActorRef,
    activity: Option<&ActivityScope>,
) -> Result<Value> {
    // Typed request boundary: malformed input and unsupported actions are
    // rejected before any approval prompt or backend side effect.
    let request = TaskRequest::parse(backend, operation, args)?;
    // The instruction is observed before approval: a denied or failed
    // dispatch still leaves the caller's ask in the journal, while
    // acceptance and outcomes are separate observations.
    observation::record_instruction(session, actor, backend, operation, &request, args);
    let result = match &request {
        TaskRequest::Status => {
            let (detail, metadata) = status_approval(backend);
            authorize(backend, operation, session, detail, metadata, activity).await?;
            backend.status(session).await
        }
        TaskRequest::Start(request) => {
            let (detail, metadata) = task_start_approval(request);
            authorize(backend, operation, session, detail, metadata, activity).await?;
            backend.task_start(args, session).await
        }
        TaskRequest::Get(_) => backend.task_get(args, session).await,
        // Read-only projection of retained state: approval-free like
        // task_get; it never reconciles or mutates.
        TaskRequest::List => backend.task_list(args, session).await,
        TaskRequest::Control(request) => {
            let (detail, metadata) = task_control_approval(backend, request);
            authorize(backend, operation, session, detail, metadata, activity).await?;
            backend.task_control(args, session).await
        }
    };
    observation::record_outcome(session, actor, backend, operation, &request, &result);
    result
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

fn task_start_approval(request: &TaskStartRequest) -> (String, BTreeMap<String, String>) {
    match &request.options {
        requests::StartOptions::Codex(options) => codex_task_start_approval(request, options),
        #[cfg(feature = "network")]
        requests::StartOptions::OpenCode(options) => opencode_task_start_approval(request, options),
        requests::StartOptions::DevinAcp(options) => devin_task_start_approval(request, options),
        #[cfg(feature = "network")]
        requests::StartOptions::DevinCloud(options) => {
            devin_cloud_task_start_approval(request, options)
        }
    }
}

fn task_control_approval(
    backend: Backend,
    request: &TaskControlRequest,
) -> (String, BTreeMap<String, String>) {
    match backend {
        Backend::Codex => codex_task_control_approval(request),
        #[cfg(feature = "network")]
        Backend::OpenCode => opencode_task_control_approval(request),
        Backend::DevinAcp => devin_task_control_approval(request),
        #[cfg(feature = "network")]
        Backend::DevinCloud => devin_cloud_task_control_approval(request),
    }
}

fn codex_status_approval() -> (String, BTreeMap<String, String>) {
    (
        "Codex delegation request\naccess: read-only\nscope: current session\nresult: model and effort compatibility metadata".to_owned(),
        approval_metadata(Backend::Codex, "codex_status", "status", false, "session_scope"),
    )
}

fn codex_task_start_approval(
    request: &TaskStartRequest,
    options: &CodexStartOptions,
) -> (String, BTreeMap<String, String>) {
    let operation_id = render_approval_argument(request.operation_id);
    let model = render_approval_argument(options.model);
    let effort = render_approval_argument(options.effort);
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

fn codex_task_control_approval(request: &TaskControlRequest) -> (String, BTreeMap<String, String>) {
    let task_id = render_approval_argument(request.task_id);
    let operation_id = render_approval_argument(request.operation_id);
    let action = request.action.as_str().to_owned();
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
fn opencode_task_start_approval(
    request: &TaskStartRequest,
    options: &OpenCodeStartOptions,
) -> (String, BTreeMap<String, String>) {
    let operation_id = render_approval_argument(request.operation_id);
    let model = render_optional_approval_argument(options.model);
    let agent = render_optional_approval_argument(options.agent);
    let variant = render_optional_approval_argument(options.variant);
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
fn opencode_task_control_approval(
    request: &TaskControlRequest,
) -> (String, BTreeMap<String, String>) {
    let task_id = render_approval_argument(request.task_id);
    let operation_id = render_approval_argument(request.operation_id);
    let action = request.action.as_str().to_owned();
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

fn devin_task_start_approval(
    request: &TaskStartRequest,
    options: &DevinAcpStartOptions,
) -> (String, BTreeMap<String, String>) {
    let operation_id = render_approval_argument(request.operation_id);
    let model = render_optional_approval_argument(options.model);
    let agent = render_optional_approval_argument(options.agent);
    let cloud = options.cloud;
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

fn devin_task_control_approval(request: &TaskControlRequest) -> (String, BTreeMap<String, String>) {
    let task_id = render_approval_argument(request.task_id);
    let operation_id = render_approval_argument(request.operation_id);
    let action = request.action.as_str().to_owned();
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
fn devin_cloud_task_start_approval(
    request: &TaskStartRequest,
    options: &DevinCloudStartOptions,
) -> (String, BTreeMap<String, String>) {
    let operation_id = render_approval_argument(request.operation_id);
    let title = render_optional_approval_argument(options.title);
    let devin_mode = render_optional_approval_argument(options.devin_mode);
    let swe_tier = render_optional_approval_argument(options.swe_tier);
    let repos = options.repos.len().to_string();
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
    metadata.insert("swe_tier".to_owned(), swe_tier.clone());
    metadata.insert("repos".to_owned(), repos.clone());
    metadata.insert("task_input".to_owned(), "omitted".to_owned());
    (
        format!(
            "Devin Cloud delegation request\noperation: start hosted session\nmutation: remote Devin Cloud session (consumes ACUs)\nscope: Devin Cloud organization, not this host\ntitle: {title}\ndevin_mode: {devin_mode}\nswe_tier: {swe_tier}\nrepos: {repos}\noperation_id: {operation_id}\ntask input: omitted"
        ),
        metadata,
    )
}

#[cfg(feature = "network")]
fn devin_cloud_task_control_approval(
    request: &TaskControlRequest,
) -> (String, BTreeMap<String, String>) {
    let task_id = render_approval_argument(request.task_id);
    let operation_id = render_approval_argument(request.operation_id);
    let action = request.action.as_str().to_owned();
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

fn render_optional_approval_argument(value: Option<&str>) -> String {
    match value {
        Some(value) => render_approval_argument(value),
        None => "(not provided)".to_owned(),
    }
}

fn render_approval_argument(value: &str) -> String {
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

/// Every backend that can answer the shared task contract, in stable
/// order for cross-backend projections.
const ALL_BACKENDS: &[Backend] = &[
    Backend::Codex,
    #[cfg(feature = "network")]
    Backend::OpenCode,
    Backend::DevinAcp,
    #[cfg(feature = "network")]
    Backend::DevinCloud,
];

/// Session-scoped task list across every enabled backend.
///
/// The backend stores remain the source of truth: this is a read-only
/// projection of the records owned by `session`'s full instance and
/// canonical scope — no second task store, no live reconciliation. One
/// backend's failure is reported per-backend (`unavailable`) rather than
/// faked as an empty list, so partial results stay distinguishable. Each
/// item carries `backend` + `task_id` so the (backend, task id, session
/// instance) reference survives the merge without reassigning ids.
///
/// Local-protocol packets wire this to a frontend; today only tests
/// reach it.
#[allow(dead_code)]
pub(crate) async fn task_list(args: &Value, session: &config::Session) -> Result<Value> {
    let limit = requests::task_list_limit(args)?;
    let mut results = Vec::with_capacity(ALL_BACKENDS.len());
    for backend in ALL_BACKENDS {
        results.push((*backend, backend.task_list(args, session).await));
    }
    Ok(merge_task_lists(results, limit))
}

/// Merge per-backend list envelopes into the common projection.
fn merge_task_lists(results: Vec<(Backend, Result<Value>)>, limit: usize) -> Value {
    let mut tasks = Vec::new();
    let mut backends = serde_json::Map::new();
    let mut total = 0usize;
    for (backend, result) in results {
        match result {
            Ok(view) => {
                let backend_total = view["total"].as_u64().unwrap_or(0) as usize;
                let skipped = view["skipped"].as_u64().unwrap_or(0) as usize;
                total += backend_total;
                if let Some(items) = view["tasks"].as_array() {
                    tasks.extend(items.iter().cloned());
                }
                backends.insert(
                    backend.name().to_owned(),
                    json!({"status": "ok", "total": backend_total, "skipped": skipped}),
                );
            }
            Err(error) => {
                backends.insert(
                    backend.name().to_owned(),
                    json!({"status": "unavailable", "error": bound_task_list_error(&error)}),
                );
            }
        }
    }
    sort_task_list_items(&mut tasks);
    let truncated = tasks.len() > limit;
    tasks.truncate(limit);
    json!({
        "tasks": tasks,
        "backends": Value::Object(backends),
        "total": total,
        "truncated": truncated,
        "limit": limit,
    })
}

/// Total deterministic order for the common projection: most recently
/// updated first, `task_id` ascending as the tie-break.
fn sort_task_list_items(tasks: &mut [Value]) {
    tasks.sort_by(|a, b| {
        let a_updated = a["last_updated_at"].as_u64().unwrap_or(0);
        let b_updated = b["last_updated_at"].as_u64().unwrap_or(0);
        b_updated.cmp(&a_updated).then_with(|| {
            a["task_id"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["task_id"].as_str().unwrap_or_default())
        })
    });
}

/// One-line, length-bounded rendering of a backend's list failure for the
/// `unavailable` marker.
fn bound_task_list_error(error: &anyhow::Error) -> String {
    let mut rendered = String::new();
    for character in format!("{error:#}").chars() {
        if rendered.len().saturating_add(character.len_utf8()) > 256 {
            rendered.push('…');
            break;
        }
        rendered.push(character);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;

    static BACKEND_DISPATCHES: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn note_backend_dispatch() {
        BACKEND_DISPATCHES.fetch_add(1, Ordering::SeqCst);
    }

    fn backend_dispatch_count() -> usize {
        BACKEND_DISPATCHES.load(Ordering::SeqCst)
    }

    fn test_session(id: &str) -> config::Session {
        let root = tempfile::tempdir().unwrap();
        let cwd = config::canonical_directory(root.path()).unwrap();
        config::Session {
            id: id.to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1234,
            process_id: 5678,
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn codex_approval_details_are_actionable_without_task_input() {
        let task_marker = "prompt-secret-marker";
        let start_args = json!({
            "operation_id": "0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa",
            "task": task_marker,
            "model": "gpt-5.6-luna",
            "effort": "max"
        });
        let TaskRequest::Start(request) =
            TaskRequest::parse(Backend::Codex, Operation::TaskStart, &start_args).unwrap()
        else {
            panic!("codex start request should parse")
        };
        let (start_detail, start_metadata) = task_start_approval(&request);
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

        let control_args = json!({
            "task_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb",
            "operation_id": "0199cccc-cccc-7ccc-8ccc-cccccccccccc",
            "action": "steer",
            "input": task_marker
        });
        let TaskRequest::Control(request) =
            TaskRequest::parse(Backend::Codex, Operation::TaskControl, &control_args).unwrap()
        else {
            panic!("codex control request should parse")
        };
        let (control_detail, control_metadata) = task_control_approval(Backend::Codex, &request);
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

    #[tokio::test]
    async fn invoke_rejects_invalid_input_before_any_backend_dispatch() {
        let session = test_session("invoke-invalid");
        let backends = [
            Backend::Codex,
            #[cfg(feature = "network")]
            Backend::OpenCode,
            Backend::DevinAcp,
            #[cfg(feature = "network")]
            Backend::DevinCloud,
        ];
        for backend in backends {
            // Malformed required input never reaches approval or dispatch.
            assert!(
                invoke(
                    backend,
                    Operation::TaskStart,
                    &json!({}),
                    &session,
                    &observation::ActorRef::mcp(false),
                    None,
                )
                .await
                .is_err()
            );
            assert!(
                invoke(
                    backend,
                    Operation::TaskGet,
                    &json!({"task_id": "not-a-uuid"}),
                    &session,
                    &observation::ActorRef::mcp(false),
                    None,
                )
                .await
                .is_err()
            );
            assert!(
                invoke(
                    backend,
                    Operation::TaskControl,
                    &json!({"task_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb"}),
                    &session,
                    &observation::ActorRef::mcp(false),
                    None,
                )
                .await
                .is_err()
            );
        }
        assert_eq!(backend_dispatch_count(), 0);
    }

    #[tokio::test]
    async fn invoke_rejects_unsupported_actions_before_any_backend_dispatch() {
        let session = test_session("invoke-unsupported");
        let backends = [
            (Backend::Codex, "Codex"),
            #[cfg(feature = "network")]
            (Backend::OpenCode, "OpenCode"),
            (Backend::DevinAcp, "Devin"),
            #[cfg(feature = "network")]
            (Backend::DevinCloud, "Devin"),
        ];
        for (backend, label) in backends {
            let error = invoke(
                backend,
                Operation::TaskControl,
                &json!({
                    "task_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb",
                    "operation_id": "0199cccc-cccc-7ccc-8ccc-cccccccccccc",
                    "action": "terminate",
                }),
                &session,
                &observation::ActorRef::mcp(false),
                None,
            )
            .await
            .unwrap_err();
            assert_eq!(
                format!("{error:#}"),
                format!("unsupported {label} task action")
            );
        }
        assert_eq!(backend_dispatch_count(), 0);
    }

    #[tokio::test]
    async fn invoke_task_list_rejects_invalid_limit_before_any_backend_dispatch() {
        let session = test_session("task-list-invalid");
        let backends = [
            Backend::Codex,
            #[cfg(feature = "network")]
            Backend::OpenCode,
            Backend::DevinAcp,
            #[cfg(feature = "network")]
            Backend::DevinCloud,
        ];
        for backend in backends {
            for args in [
                json!({"limit": 0}),
                json!({"limit": 129}),
                json!({"limit": "many"}),
            ] {
                assert!(
                    invoke(
                        backend,
                        Operation::TaskList,
                        &args,
                        &session,
                        &observation::ActorRef::mcp(false),
                        None,
                    )
                    .await
                    .is_err()
                );
            }
        }
        assert_eq!(backend_dispatch_count(), 0);
    }

    #[test]
    fn task_list_merge_keeps_unavailable_backends_visible() {
        let ok = Ok(json!({
            "backend": "codex",
            "tasks": [
                {"task_id": "b-task", "backend": "codex", "last_updated_at": 20},
                {"task_id": "a-task", "backend": "codex", "last_updated_at": 30},
            ],
            "total": 2,
            "skipped": 1,
        }));
        let unavailable = Err(anyhow::anyhow!("store offline"));
        let view = merge_task_lists(
            vec![(Backend::Codex, ok), (Backend::DevinAcp, unavailable)],
            50,
        );

        assert_eq!(view["total"], 2);
        assert_eq!(view["truncated"], false);
        let tasks = view["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        // Newest first across the whole projection.
        assert_eq!(tasks[0]["task_id"], "a-task");
        assert_eq!(tasks[1]["task_id"], "b-task");
        assert_eq!(view["backends"]["codex"]["status"], "ok");
        assert_eq!(view["backends"]["codex"]["total"], 2);
        assert_eq!(view["backends"]["codex"]["skipped"], 1);
        assert_eq!(view["backends"]["devin_acp"]["status"], "unavailable");
        assert!(
            view["backends"]["devin_acp"]["error"]
                .as_str()
                .unwrap()
                .contains("store offline"),
            "a failed backend reports its error instead of an empty list"
        );
    }

    #[test]
    fn task_list_merge_orders_deterministically_and_truncates() {
        let ok_a = Ok(json!({
            "tasks": [
                {"task_id": "same-time-b", "backend": "codex", "last_updated_at": 50},
                {"task_id": "same-time-a", "backend": "devin_acp", "last_updated_at": 50},
                {"task_id": "old", "backend": "codex", "last_updated_at": 10},
            ],
            "total": 3,
            "skipped": 0,
        }));
        let ok_b = Ok(json!({
            "tasks": [{"task_id": "newest", "backend": "devin_acp", "last_updated_at": 60}],
            "total": 1,
            "skipped": 0,
        }));
        let view = merge_task_lists(vec![(Backend::Codex, ok_a), (Backend::DevinAcp, ok_b)], 3);

        assert_eq!(view["total"], 4);
        assert_eq!(view["truncated"], true);
        assert_eq!(view["limit"], 3);
        let tasks = view["tasks"].as_array().unwrap();
        let order: Vec<&str> = tasks
            .iter()
            .map(|task| task["task_id"].as_str().unwrap())
            .collect();
        // updated_at descending, task_id ascending as the tie-break.
        assert_eq!(order, ["newest", "same-time-a", "same-time-b"]);
    }
}
