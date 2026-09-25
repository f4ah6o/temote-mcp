//! Typed request and capability boundary for task operations.
//!
//! Frontends hand raw operation input to [`TaskRequest::parse`]. Parsing
//! mirrors each backend's own argument validation — same fields, same
//! order, same error strings — so malformed input and unsupported actions
//! are rejected here, before any approval prompt or backend side effect.
//! The typed request keeps backend-specific options as typed variants and
//! keeps capability differences (hosted execution, resume support, the
//! meaning of interrupt, wait-for-input) as data instead of flattening
//! them into the common contract.
//!
//! Task and control input text is validated but intentionally not
//! retained on the request: approvals and evidence render it as `omitted`.

use anyhow::{Context, Result};
use serde_json::Value;
use uuid::Uuid;

use super::{Backend, Operation};

// Mirrors of the backend argument limits; the validation tests lock the
// rendered error strings so these cannot silently diverge.
const MAX_TASK_INPUT_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 256;
#[cfg(feature = "network")]
const MAX_REPOS: usize = 16;
const DEFAULT_TASK_LIST_LIMIT: usize = 50;
const MAX_TASK_LIST_LIMIT: u64 = 128;

/// One typed operation on the shared task contract, with backend-specific
/// input preserved as typed options.
#[derive(Debug)]
pub(crate) enum TaskRequest<'a> {
    Status,
    Start(TaskStartRequest<'a>),
    // Retained fields are consumed by the shared task index planned in a
    // follow-up packet; dispatch reads them from the raw args for now.
    #[allow(dead_code)]
    Get(TaskGetRequest<'a>),
    /// A session-scoped read-only projection of the caller's retained
    /// tasks. `limit` is validated at the boundary like every other
    /// input; the dispatch then reads it from the raw args, so nothing
    /// is retained on the request.
    List,
    Control(TaskControlRequest<'a>),
}

/// A validated `task_start` request. `operation_id` is the caller's
/// verbatim UUID string (validated at parse and rendered exactly in
/// approvals). The `task` text is validated but not retained.
#[derive(Debug)]
pub(crate) struct TaskStartRequest<'a> {
    pub(crate) operation_id: &'a str,
    pub(crate) options: StartOptions<'a>,
}

/// The backend-specific portion of a start request. Common fields live on
/// [`TaskStartRequest`]; these are the options each backend defines on top
/// of them.
#[derive(Debug)]
pub(crate) enum StartOptions<'a> {
    Codex(CodexStartOptions<'a>),
    #[cfg(feature = "network")]
    OpenCode(OpenCodeStartOptions<'a>),
    DevinAcp(DevinAcpStartOptions<'a>),
    #[cfg(feature = "network")]
    DevinCloud(DevinCloudStartOptions<'a>),
}

/// Codex app-server start options: required model and effort selectors.
#[derive(Debug)]
pub(crate) struct CodexStartOptions<'a> {
    pub(crate) model: &'a str,
    pub(crate) effort: &'a str,
}

/// `opencode serve` start options.
#[cfg(feature = "network")]
#[derive(Debug)]
pub(crate) struct OpenCodeStartOptions<'a> {
    /// `provider/model` selector, e.g. `anthropic/claude-sonnet-4`.
    pub(crate) model: Option<&'a str>,
    pub(crate) agent: Option<&'a str>,
    pub(crate) variant: Option<&'a str>,
}

/// Devin ACP (`devin acp`) start options. `cloud` selects the hosted
/// relay: `devin acp --cloud` sessions run on hosted execution, never as
/// a local child process, so the local selectors (`model`/`agent`) are
/// rejected when it is set.
#[derive(Debug)]
pub(crate) struct DevinAcpStartOptions<'a> {
    pub(crate) model: Option<&'a str>,
    pub(crate) agent: Option<&'a str>,
    pub(crate) cloud: bool,
}

/// Devin Cloud API start options.
#[cfg(feature = "network")]
#[derive(Debug)]
pub(crate) struct DevinCloudStartOptions<'a> {
    pub(crate) title: Option<&'a str>,
    pub(crate) devin_mode: Option<&'a str>,
    pub(crate) repos: Vec<String>,
    // Validated (1..=100000) at parse; approvals do not render it today,
    // so it stays boundary data until a consumer reads it.
    #[allow(dead_code)]
    pub(crate) max_acu_limit: Option<u64>,
}

/// A validated `task_get` request.
#[derive(Debug)]
pub(crate) struct TaskGetRequest<'a> {
    // Retained on the typed request for the shared task index planned in
    // a follow-up packet; the current dispatch reads task ids from the
    // raw args.
    #[allow(dead_code)]
    pub(crate) task_id: &'a str,
    #[allow(dead_code)]
    pub(crate) after_revision: Option<u64>,
}

/// A typed control action. `steer` requires input text; `resume` and
/// `interrupt` accept none — enforced at parse time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlAction {
    Steer,
    Resume,
    Interrupt,
}

impl ControlAction {
    /// The canonical spelling, identical to the accepted wire values.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ControlAction::Steer => "steer",
            ControlAction::Resume => "resume",
            ControlAction::Interrupt => "interrupt",
        }
    }
}

/// A validated `task_control` request. The `input` text is validated at
/// parse but not retained — approvals render it as `omitted`.
#[derive(Debug)]
pub(crate) struct TaskControlRequest<'a> {
    pub(crate) task_id: &'a str,
    pub(crate) operation_id: &'a str,
    pub(crate) action: ControlAction,
}

/// Where task execution runs relative to this host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecutionLocality {
    /// A child process on this host inside the session's cwd scope.
    HostLocal,
    /// A remote hosted session; nothing runs on this host.
    Hosted,
}

/// What a `resume` control action means on a backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumeSemantics {
    /// Re-derive the retained thread's state after an uncertain gap
    /// (Codex `thread/read` reconciliation).
    ReconcileThread,
    /// Send resume instructions to the retained backend session
    /// (OpenCode serve; the Devin Cloud hosted session).
    /// Only network backends resume this way.
    #[allow(dead_code)]
    ResumeMessage,
    /// Reattach via `session/load`, honored only when the agent
    /// advertised `loadSession` at initialize; without it the backend
    /// fails closed at runtime.
    AgentLoadSession,
}

/// What an `interrupt` control action stops on a backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InterruptSemantics {
    /// Cancel the in-flight turn; the agent thread survives
    /// (Codex `turn/interrupt`).
    CancelTurn,
    /// Abort or cancel the backend session bound to the task
    /// (OpenCode `abort`; Devin ACP `session/cancel`).
    AbortSession,
    /// Terminate the hosted session entirely (Devin Cloud).
    /// Only the network-gated Devin Cloud backend interrupts this way.
    #[allow(dead_code)]
    TerminateHosted,
}

/// The wait-for-input states a backend can surface through task views.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputWait {
    /// Agent-side approval waits only (`waiting_approval`).
    ApprovalOnly,
    /// Hosted sessions may also wait for user input (`waiting_for_user`,
    /// surfaced as `waiting_input` and resolvable by a follow-up
    /// message). Only the network-gated Devin Cloud backend declares it.
    #[allow(dead_code)]
    UserInput,
}

/// The static capability boundary of a backend. These differences stay
/// typed data rather than being flattened into the shared contract.
/// Per-request overrides (for example `devin acp --cloud` switching a
/// Devin ACP task to hosted execution) resolve on the typed request via
/// [`StartOptions::execution_locality`].
#[derive(Debug)]
pub(crate) struct BackendCapabilities {
    /// Where executions on this backend run by default.
    pub(crate) execution: ExecutionLocality,
    /// `None` when the backend has no resume action.
    pub(crate) resume: Option<ResumeSemantics>,
    /// `None` when the backend has no interrupt action.
    pub(crate) interrupt: Option<InterruptSemantics>,
    /// Wait-for-input states the backend can surface. Declared for the
    /// shared boundary; the MCP adapter does not read it yet.
    #[allow(dead_code)]
    pub(crate) input_wait: InputWait,
}

impl BackendCapabilities {
    /// Whether `action` is part of this backend's typed control contract.
    /// The core rejects unsupported actions at parse time, before any
    /// approval prompt or backend side effect.
    pub(crate) fn supports_control_action(&self, action: ControlAction) -> bool {
        match action {
            ControlAction::Steer => true,
            ControlAction::Resume => self.resume.is_some(),
            ControlAction::Interrupt => self.interrupt.is_some(),
        }
    }
}

impl Backend {
    /// The static capability boundary of this backend.
    pub(crate) fn capabilities(self) -> BackendCapabilities {
        match self {
            Backend::Codex => BackendCapabilities {
                execution: ExecutionLocality::HostLocal,
                resume: Some(ResumeSemantics::ReconcileThread),
                interrupt: Some(InterruptSemantics::CancelTurn),
                input_wait: InputWait::ApprovalOnly,
            },
            #[cfg(feature = "network")]
            Backend::OpenCode => BackendCapabilities {
                execution: ExecutionLocality::HostLocal,
                resume: Some(ResumeSemantics::ResumeMessage),
                interrupt: Some(InterruptSemantics::AbortSession),
                input_wait: InputWait::ApprovalOnly,
            },
            Backend::DevinAcp => BackendCapabilities {
                execution: ExecutionLocality::HostLocal,
                resume: Some(ResumeSemantics::AgentLoadSession),
                interrupt: Some(InterruptSemantics::AbortSession),
                input_wait: InputWait::ApprovalOnly,
            },
            #[cfg(feature = "network")]
            Backend::DevinCloud => BackendCapabilities {
                execution: ExecutionLocality::Hosted,
                resume: Some(ResumeSemantics::ResumeMessage),
                interrupt: Some(InterruptSemantics::TerminateHosted),
                input_wait: InputWait::UserInput,
            },
        }
    }

    /// The label used by control-action errors (e.g. "unsupported Codex
    /// task action").
    fn action_label(self) -> &'static str {
        match self {
            Backend::Codex => "Codex",
            #[cfg(feature = "network")]
            Backend::OpenCode => "OpenCode",
            Backend::DevinAcp => "Devin",
            #[cfg(feature = "network")]
            Backend::DevinCloud => "Devin",
        }
    }
}

impl StartOptions<'_> {
    /// Where this specific start would execute. `devin acp --cloud` is
    /// hosted execution, not a local child process — the flag lives on
    /// the typed request so a per-request hosted start can never be
    /// flattened into the backend's host-local default.
    pub(crate) fn execution_locality(&self) -> ExecutionLocality {
        match self {
            #[cfg(feature = "network")]
            StartOptions::DevinCloud(_) => ExecutionLocality::Hosted,
            StartOptions::DevinAcp(options) if options.cloud => ExecutionLocality::Hosted,
            _ => ExecutionLocality::HostLocal,
        }
    }
}

impl<'a> TaskRequest<'a> {
    /// Parse raw operation input onto the typed request boundary. Every
    /// rejection mirrors the backend's own validation error, but happens
    /// before any approval prompt or backend side effect.
    pub(crate) fn parse(backend: Backend, operation: Operation, args: &'a Value) -> Result<Self> {
        Ok(match operation {
            Operation::Status => TaskRequest::Status,
            Operation::TaskStart => TaskRequest::Start(parse_task_start(backend, args)?),
            Operation::TaskGet => TaskRequest::Get(parse_task_get(backend, args)?),
            Operation::TaskList => {
                task_list_limit(args)?;
                TaskRequest::List
            }
            Operation::TaskControl => TaskRequest::Control(parse_task_control(backend, args)?),
        })
    }
}

// The argument helpers below mirror each backend's own validators —
// same checks, same order, same error strings — so a request rejected
// here fails identically to one rejected inside the backend.

fn required_uuid<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    let value = required_string(args, key)?;
    Uuid::parse_str(value).with_context(|| format!("{key} must be a UUID"))?;
    Ok(value)
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing or invalid {key}"))
}

fn optional_string<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .with_context(|| format!("{key} must be a string")),
    }
}

/// `optional_u64` as the local backends implement it: a present `null`
/// is an error.
fn optional_u64_strict(args: &Value, key: &str) -> Result<Option<u64>> {
    args.get(key)
        .map(|value| {
            value
                .as_u64()
                .with_context(|| format!("{key} must be a non-negative integer"))
        })
        .transpose()
}

/// `optional_u64` as Devin Cloud implements it: a present `null` reads
/// as absent.
#[cfg(feature = "network")]
fn optional_u64_lenient(args: &Value, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .with_context(|| format!("{key} must be a non-negative integer")),
    }
}

#[cfg(feature = "network")]
fn optional_string_list(args: &Value, key: &str, max: usize) -> Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            anyhow::ensure!(items.len() <= max, "{key} accepts at most {max} entries");
            items
                .iter()
                .map(|item| {
                    let value = item
                        .as_str()
                        .with_context(|| format!("{key} entries must be strings"))?;
                    validate_argument_control_free(value, key)?;
                    Ok(value.to_owned())
                })
                .collect()
        }
        Some(_) => anyhow::bail!("{key} must be an array of strings"),
    }
}

fn validate_task_input(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_TASK_INPUT_BYTES && !value.contains('\0'),
        "{label} must contain 1..={MAX_TASK_INPUT_BYTES} NUL-free UTF-8 bytes"
    );
    Ok(())
}

fn validate_argument(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_ARGUMENT_BYTES && !value.contains('\0'),
        "{label} must contain 1..={MAX_ARGUMENT_BYTES} NUL-free UTF-8 bytes"
    );
    Ok(())
}

/// Devin Cloud applies a stricter rule than the local backends: its
/// argument fields reject every control character, not only NUL.
#[cfg(feature = "network")]
fn validate_argument_control_free(value: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= MAX_ARGUMENT_BYTES
            && !value.chars().any(char::is_control),
        "{label} must contain 1..={MAX_ARGUMENT_BYTES} control-free UTF-8 bytes"
    );
    Ok(())
}

#[cfg(feature = "network")]
fn validate_devin_mode(value: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(
            value,
            "normal"
                | "fast"
                | "lite"
                | "ultra"
                | "fusion"
                | "swe-2-medium"
                | "swe-2-high"
                | "swe-2-max"
        ),
        "devin_mode must be one of normal, fast, lite, ultra, fusion, swe-2-medium, swe-2-high, swe-2-max"
    );
    Ok(())
}

fn parse_task_start<'a>(backend: Backend, args: &'a Value) -> Result<TaskStartRequest<'a>> {
    let operation_id = required_uuid(args, "operation_id")?;
    let task = required_string(args, "task")?;
    let options = match backend {
        Backend::Codex => {
            let model = required_string(args, "model")?;
            let effort = required_string(args, "effort")?;
            validate_task_input(task, "task")?;
            validate_argument(model, "model")?;
            validate_argument(effort, "effort")?;
            StartOptions::Codex(CodexStartOptions { model, effort })
        }
        #[cfg(feature = "network")]
        Backend::OpenCode => {
            let model = optional_string(args, "model")?;
            let agent = optional_string(args, "agent")?;
            let variant = optional_string(args, "variant")?;
            validate_task_input(task, "task")?;
            if let Some(model) = model {
                validate_argument(model, "model")?;
                anyhow::ensure!(
                    model.contains('/'),
                    "model must be a provider/model pair such as anthropic/claude-sonnet-4"
                );
            }
            if let Some(agent) = agent {
                validate_argument(agent, "agent")?;
            }
            if let Some(variant) = variant {
                validate_argument(variant, "variant")?;
            }
            StartOptions::OpenCode(OpenCodeStartOptions {
                model,
                agent,
                variant,
            })
        }
        Backend::DevinAcp => {
            let model = optional_string(args, "model")?;
            let agent = optional_string(args, "agent")?;
            let cloud = args
                .get("cloud")
                .map(|value| value.as_bool().context("cloud must be a boolean"))
                .transpose()?
                .unwrap_or(false);
            validate_task_input(task, "task")?;
            if let Some(model) = model {
                validate_argument(model, "model")?;
            }
            if let Some(agent) = agent {
                validate_argument(agent, "agent")?;
            }
            let options = StartOptions::DevinAcp(DevinAcpStartOptions {
                model,
                agent,
                cloud,
            });
            anyhow::ensure!(
                options.execution_locality() == ExecutionLocality::HostLocal
                    || (model.is_none() && agent.is_none()),
                "model and agent are ignored by `devin acp --cloud`; omit them when cloud is true"
            );
            options
        }
        #[cfg(feature = "network")]
        Backend::DevinCloud => {
            let title = optional_string(args, "title")?;
            let devin_mode = optional_string(args, "devin_mode")?;
            let repos = optional_string_list(args, "repos", MAX_REPOS)?;
            let max_acu_limit = optional_u64_lenient(args, "max_acu_limit")?;
            validate_task_input(task, "task")?;
            if let Some(title) = title {
                validate_argument_control_free(title, "title")?;
            }
            if let Some(mode) = devin_mode {
                validate_devin_mode(mode)?;
            }
            if let Some(limit) = max_acu_limit {
                anyhow::ensure!(
                    (1..=100_000).contains(&limit),
                    "max_acu_limit must be within 1..=100000"
                );
            }
            StartOptions::DevinCloud(DevinCloudStartOptions {
                title,
                devin_mode,
                repos,
                max_acu_limit,
            })
        }
    };
    Ok(TaskStartRequest {
        operation_id,
        options,
    })
}

fn parse_task_get<'a>(backend: Backend, args: &'a Value) -> Result<TaskGetRequest<'a>> {
    let task_id = required_uuid(args, "task_id")?;
    let after_revision = match backend {
        #[cfg(feature = "network")]
        Backend::DevinCloud => optional_u64_lenient(args, "after_revision")?,
        _ => optional_u64_strict(args, "after_revision")?,
    };
    Ok(TaskGetRequest {
        task_id,
        after_revision,
    })
}

/// The shared task-list `limit`: optional, an integer within `1..=128`,
/// defaulting to 50 — the same bound `job_list` uses for the current
/// session's job list. Parsing mirrors the backends' own validation.
pub(crate) fn task_list_limit(args: &Value) -> Result<usize> {
    match args.get("limit") {
        None => Ok(DEFAULT_TASK_LIST_LIMIT),
        Some(value) => {
            let limit = value
                .as_u64()
                .context("task_list limit must be an integer")?;
            anyhow::ensure!(
                (1..=MAX_TASK_LIST_LIMIT).contains(&limit),
                "task_list limit must be 1..={MAX_TASK_LIST_LIMIT}"
            );
            Ok(limit as usize)
        }
    }
}

fn parse_task_control<'a>(backend: Backend, args: &'a Value) -> Result<TaskControlRequest<'a>> {
    let task_id = required_uuid(args, "task_id")?;
    let operation_id = required_uuid(args, "operation_id")?;
    let action = required_string(args, "action")?;
    let action = match action {
        "steer" => ControlAction::Steer,
        "resume" => ControlAction::Resume,
        "interrupt" => ControlAction::Interrupt,
        _ => anyhow::bail!("unsupported {} task action", backend.action_label()),
    };
    // An action outside the backend's contract is rejected here rather
    // than inside the backend, before any approval prompt or side
    // effect.
    anyhow::ensure!(
        backend.capabilities().supports_control_action(action),
        "unsupported {} task action",
        backend.action_label()
    );
    let input = args.get("input").and_then(Value::as_str);
    match action {
        ControlAction::Steer => {
            validate_task_input(input.context("steer requires input")?, "input")?
        }
        ControlAction::Resume | ControlAction::Interrupt => {
            anyhow::ensure!(input.is_none(), "{} does not accept input", action.as_str())
        }
    }
    Ok(TaskControlRequest {
        task_id,
        operation_id,
        action,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const OP_ID: &str = "0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa";
    const TASK_ID: &str = "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb";

    fn backends() -> Vec<Backend> {
        vec![
            Backend::Codex,
            #[cfg(feature = "network")]
            Backend::OpenCode,
            Backend::DevinAcp,
            #[cfg(feature = "network")]
            Backend::DevinCloud,
        ]
    }

    fn parse_start(backend: Backend, args: &Value) -> Result<TaskRequest<'_>> {
        TaskRequest::parse(backend, Operation::TaskStart, args)
    }

    fn parse_get(backend: Backend, args: &Value) -> Result<TaskRequest<'_>> {
        TaskRequest::parse(backend, Operation::TaskGet, args)
    }

    fn parse_control(backend: Backend, args: &Value) -> Result<TaskRequest<'_>> {
        TaskRequest::parse(backend, Operation::TaskControl, args)
    }

    fn parse_list(backend: Backend, args: &Value) -> Result<TaskRequest<'_>> {
        TaskRequest::parse(backend, Operation::TaskList, args)
    }

    fn error_of(result: Result<TaskRequest<'_>>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn task_start_rejects_missing_and_invalid_common_fields() {
        for backend in backends() {
            assert_eq!(
                error_of(parse_start(backend, &json!({}))),
                "missing or invalid operation_id"
            );
            assert_eq!(
                error_of(parse_start(backend, &json!({"operation_id": 42}))),
                "missing or invalid operation_id"
            );
            assert_eq!(
                error_of(parse_start(backend, &json!({"operation_id": "nope"}))),
                "operation_id must be a UUID"
            );
            assert_eq!(
                error_of(parse_start(backend, &json!({"operation_id": OP_ID}))),
                "missing or invalid task"
            );
            for task in [
                json!(""),
                json!("x".repeat(MAX_TASK_INPUT_BYTES + 1)),
                json!("has\0nul"),
            ] {
                // Codex extracts its required options before validating
                // the task text; supply them so the task error surfaces.
                let mut args = json!({"operation_id": OP_ID, "task": task});
                if backend == Backend::Codex {
                    args["model"] = json!("gpt");
                    args["effort"] = json!("high");
                }
                assert_eq!(
                    error_of(parse_start(backend, &args)),
                    "task must contain 1..=1048576 NUL-free UTF-8 bytes"
                );
            }
        }
    }

    #[test]
    fn task_start_keeps_codex_required_options() {
        let args = json!({"operation_id": OP_ID, "task": "work", "model": "gpt", "effort": "high"});
        let TaskRequest::Start(request) = parse_start(Backend::Codex, &args).unwrap() else {
            panic!("codex start should parse")
        };
        assert_eq!(request.operation_id, OP_ID);
        let StartOptions::Codex(options) = &request.options else {
            panic!("codex options expected")
        };
        assert_eq!((options.model, options.effort), ("gpt", "high"));

        assert_eq!(
            error_of(parse_start(
                Backend::Codex,
                &json!({"operation_id": OP_ID, "task": "work"})
            )),
            "missing or invalid model"
        );
        assert_eq!(
            error_of(parse_start(
                Backend::Codex,
                &json!({"operation_id": OP_ID, "task": "work", "model": "gpt"})
            )),
            "missing or invalid effort"
        );
        for (key, value) in [
            ("model", json!("")),
            ("model", json!("a\0b")),
            ("effort", json!("")),
        ] {
            let mut args =
                json!({"operation_id": OP_ID, "task": "work", "model": "gpt", "effort": "high"});
            args[key] = value;
            assert_eq!(
                error_of(parse_start(Backend::Codex, &args)),
                format!("{key} must contain 1..=256 NUL-free UTF-8 bytes")
            );
        }
    }

    #[cfg(feature = "network")]
    #[test]
    fn task_start_keeps_opencode_options() {
        let args = json!({
            "operation_id": OP_ID,
            "task": "work",
            "model": "anthropic/claude-sonnet-4",
            "agent": "build",
            "variant": "fast"
        });
        let TaskRequest::Start(request) = parse_start(Backend::OpenCode, &args).unwrap() else {
            panic!("opencode start should parse")
        };
        let StartOptions::OpenCode(options) = &request.options else {
            panic!("opencode options expected")
        };
        assert_eq!(
            (options.model, options.agent, options.variant),
            (
                Some("anthropic/claude-sonnet-4"),
                Some("build"),
                Some("fast")
            )
        );

        // Every option is optional for this backend.
        let minimal = json!({"operation_id": OP_ID, "task": "work"});
        let TaskRequest::Start(request) = parse_start(Backend::OpenCode, &minimal).unwrap() else {
            panic!("opencode minimal start should parse")
        };
        let StartOptions::OpenCode(options) = &request.options else {
            panic!("opencode options expected")
        };
        assert_eq!(
            (options.model, options.agent, options.variant),
            (None, None, None)
        );

        assert_eq!(
            error_of(parse_start(
                Backend::OpenCode,
                &json!({"operation_id": OP_ID, "task": "work", "model": "claude"})
            )),
            "model must be a provider/model pair such as anthropic/claude-sonnet-4"
        );
        assert_eq!(
            error_of(parse_start(
                Backend::OpenCode,
                &json!({"operation_id": OP_ID, "task": "work", "model": 42})
            )),
            "model must be a string"
        );
    }

    #[test]
    fn task_start_keeps_devin_acp_options_and_cloud_locality() {
        let local =
            json!({"operation_id": OP_ID, "task": "work", "model": "swe", "agent": "devin"});
        let TaskRequest::Start(request) = parse_start(Backend::DevinAcp, &local).unwrap() else {
            panic!("devin acp start should parse")
        };
        let StartOptions::DevinAcp(options) = &request.options else {
            panic!("devin acp options expected")
        };
        assert_eq!((options.model, options.agent), (Some("swe"), Some("devin")));
        assert!(!options.cloud);
        assert_eq!(
            request.options.execution_locality(),
            ExecutionLocality::HostLocal
        );

        // `devin acp --cloud` is hosted execution, and local selectors
        // are rejected there — it must never be treated as host-local.
        let cloud = json!({"operation_id": OP_ID, "task": "work", "cloud": true});
        let TaskRequest::Start(request) = parse_start(Backend::DevinAcp, &cloud).unwrap() else {
            panic!("devin acp cloud start should parse")
        };
        assert_eq!(
            request.options.execution_locality(),
            ExecutionLocality::Hosted
        );
        assert_eq!(
            error_of(parse_start(
                Backend::DevinAcp,
                &json!({"operation_id": OP_ID, "task": "work", "cloud": true, "model": "swe"})
            )),
            "model and agent are ignored by `devin acp --cloud`; omit them when cloud is true"
        );
        assert_eq!(
            error_of(parse_start(
                Backend::DevinAcp,
                &json!({"operation_id": OP_ID, "task": "work", "cloud": "yes"})
            )),
            "cloud must be a boolean"
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn task_start_keeps_devin_cloud_options() {
        let args = json!({
            "operation_id": OP_ID,
            "task": "work",
            "title": "My task",
            "devin_mode": "ultra",
            "repos": ["org/one", "org/two"],
            "max_acu_limit": 42
        });
        let TaskRequest::Start(request) = parse_start(Backend::DevinCloud, &args).unwrap() else {
            panic!("devin cloud start should parse")
        };
        let StartOptions::DevinCloud(options) = &request.options else {
            panic!("devin cloud options expected")
        };
        assert_eq!(
            (options.title, options.devin_mode, options.max_acu_limit),
            (Some("My task"), Some("ultra"), Some(42))
        );
        assert_eq!(
            options.repos,
            vec!["org/one".to_owned(), "org/two".to_owned()]
        );
        assert_eq!(
            request.options.execution_locality(),
            ExecutionLocality::Hosted
        );

        // Devin Cloud fields reject any control character — a stricter
        // rule than the local backends' NUL-only check.
        let cases = [
            (
                json!({"devin_mode": "bogus"}),
                "devin_mode must be one of normal, fast, lite, ultra, fusion, swe-2-medium, swe-2-high, swe-2-max",
            ),
            (json!({"repos": 42}), "repos must be an array of strings"),
            (
                json!({"repos": ["org/one", 5]}),
                "repos entries must be strings",
            ),
            (
                json!({"repos": ["bad\nrepo"]}),
                "repos must contain 1..=256 control-free UTF-8 bytes",
            ),
            (
                json!({"title": "has\ttab"}),
                "title must contain 1..=256 control-free UTF-8 bytes",
            ),
            (
                json!({"max_acu_limit": 0}),
                "max_acu_limit must be within 1..=100000",
            ),
            (
                json!({"max_acu_limit": 100_001}),
                "max_acu_limit must be within 1..=100000",
            ),
        ];
        for (extra, expected) in cases {
            let mut args = json!({"operation_id": OP_ID, "task": "work"});
            args.as_object_mut().unwrap().extend(
                extra
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            assert_eq!(error_of(parse_start(Backend::DevinCloud, &args)), expected);
        }

        let mut args = json!({"operation_id": OP_ID, "task": "work"});
        args["repos"] = json!(vec!["org/repo"; MAX_REPOS + 1]);
        assert_eq!(
            error_of(parse_start(Backend::DevinCloud, &args)),
            "repos accepts at most 16 entries"
        );

        // `null` reads as absent for this backend's optional fields.
        assert!(
            parse_start(
                Backend::DevinCloud,
                &json!({"operation_id": OP_ID, "task": "work", "max_acu_limit": null})
            )
            .is_ok()
        );
    }

    #[test]
    fn task_get_rejects_invalid_input() {
        for backend in backends() {
            assert_eq!(
                error_of(parse_get(backend, &json!({}))),
                "missing or invalid task_id"
            );
            assert_eq!(
                error_of(parse_get(backend, &json!({"task_id": "nope"}))),
                "task_id must be a UUID"
            );
            assert_eq!(
                error_of(parse_get(
                    backend,
                    &json!({"task_id": TASK_ID, "after_revision": -1})
                )),
                "after_revision must be a non-negative integer"
            );
        }
    }

    #[test]
    fn task_get_after_revision_null_is_backend_specific() {
        // Devin Cloud's optional_u64 reads a present null as absent; the
        // local backends reject it. The typed boundary preserves both.
        for backend in backends() {
            let args = json!({"task_id": TASK_ID, "after_revision": null});
            match backend {
                #[cfg(feature = "network")]
                Backend::DevinCloud => assert!(parse_get(backend, &args).is_ok()),
                _ => assert_eq!(
                    error_of(parse_get(backend, &args)),
                    "after_revision must be a non-negative integer"
                ),
            }
        }
    }

    #[test]
    fn task_control_typed_actions_and_labels() {
        let labels: Vec<(Backend, &str)> = vec![
            (Backend::Codex, "Codex"),
            #[cfg(feature = "network")]
            (Backend::OpenCode, "OpenCode"),
            (Backend::DevinAcp, "Devin"),
            #[cfg(feature = "network")]
            (Backend::DevinCloud, "Devin"),
        ];
        for (backend, label) in labels {
            assert_eq!(
                error_of(parse_control(
                    backend,
                    &json!({"task_id": TASK_ID, "operation_id": OP_ID})
                )),
                "missing or invalid action"
            );
            assert_eq!(
                error_of(parse_control(
                    backend,
                    &json!({"task_id": TASK_ID, "operation_id": OP_ID, "action": "bogus"})
                )),
                format!("unsupported {label} task action")
            );
            for action in ["steer", "resume", "interrupt"] {
                let args = json!({
                    "task_id": TASK_ID,
                    "operation_id": OP_ID,
                    "action": action,
                    "input": if action == "steer" { json!("go") } else { json!(null) },
                });
                let result = parse_control(backend, &args);
                if action == "steer" {
                    let TaskRequest::Control(request) = result.unwrap() else {
                        panic!("steer should parse")
                    };
                    assert_eq!(request.action, ControlAction::Steer);
                    assert_eq!(request.task_id, TASK_ID);
                    assert_eq!(request.operation_id, OP_ID);
                } else {
                    assert!(result.is_ok());
                }
            }
        }
    }

    #[test]
    fn task_control_input_rules_are_action_specific() {
        let args = |extra: Value| {
            let mut args = json!({"task_id": TASK_ID, "operation_id": OP_ID, "action": "steer"});
            args.as_object_mut().unwrap().extend(
                extra
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            args
        };
        assert_eq!(
            error_of(parse_control(Backend::Codex, &args(json!({})))),
            "steer requires input"
        );
        // A non-string input reads as absent — the backend treats it the
        // same way.
        assert_eq!(
            error_of(parse_control(Backend::Codex, &args(json!({"input": 42})))),
            "steer requires input"
        );
        assert_eq!(
            error_of(parse_control(Backend::Codex, &args(json!({"input": ""})))),
            "input must contain 1..=1048576 NUL-free UTF-8 bytes"
        );
        for action in ["resume", "interrupt"] {
            assert_eq!(
                error_of(parse_control(
                    Backend::Codex,
                    &args(json!({"action": action, "input": "x"}))
                )),
                format!("{action} does not accept input")
            );
        }
    }

    #[test]
    fn capabilities_keep_backend_differences_typed() {
        for backend in backends() {
            let capabilities = backend.capabilities();
            let expected_execution = match backend {
                Backend::Codex | Backend::DevinAcp => ExecutionLocality::HostLocal,
                #[cfg(feature = "network")]
                Backend::OpenCode => ExecutionLocality::HostLocal,
                #[cfg(feature = "network")]
                Backend::DevinCloud => ExecutionLocality::Hosted,
            };
            assert_eq!(capabilities.execution, expected_execution);
            assert!(capabilities.resume.is_some());
            assert!(capabilities.interrupt.is_some());
            for action in [
                ControlAction::Steer,
                ControlAction::Resume,
                ControlAction::Interrupt,
            ] {
                assert!(capabilities.supports_control_action(action));
            }
        }
        assert_eq!(
            Backend::Codex.capabilities().resume,
            Some(ResumeSemantics::ReconcileThread)
        );
        assert_eq!(
            Backend::DevinAcp.capabilities().resume,
            Some(ResumeSemantics::AgentLoadSession)
        );
        #[cfg(feature = "network")]
        {
            assert_eq!(
                Backend::DevinCloud.capabilities().interrupt,
                Some(InterruptSemantics::TerminateHosted)
            );
            assert_eq!(
                Backend::OpenCode.capabilities().interrupt,
                Some(InterruptSemantics::AbortSession)
            );
        }
    }

    #[test]
    fn task_list_validates_the_limit_at_the_boundary() {
        for backend in backends() {
            let TaskRequest::List = parse_list(backend, &json!({})).unwrap() else {
                panic!("task_list should parse")
            };
            assert!(matches!(
                parse_list(backend, &json!({"limit": 1})).unwrap(),
                TaskRequest::List
            ));
            assert!(matches!(
                parse_list(backend, &json!({"limit": 128})).unwrap(),
                TaskRequest::List
            ));
            assert_eq!(
                error_of(parse_list(backend, &json!({"limit": "8"}))),
                "task_list limit must be an integer"
            );
            assert_eq!(
                error_of(parse_list(backend, &json!({"limit": 0}))),
                "task_list limit must be 1..=128"
            );
            assert_eq!(
                error_of(parse_list(backend, &json!({"limit": 129}))),
                "task_list limit must be 1..=128"
            );
        }
    }

    #[test]
    fn task_list_limit_defaults_and_bounds() {
        assert_eq!(task_list_limit(&json!({})).unwrap(), 50);
        assert_eq!(task_list_limit(&json!({"limit": 64})).unwrap(), 64);
    }
}
