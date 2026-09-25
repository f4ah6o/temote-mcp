//! Owner-only observation journal (issue packets O1 + O2).
//!
//! Observation happens at Temote's single observable boundary — the shared
//! orchestration entry — so the same semantic operation records the same
//! shape regardless of transport. Records are append-only JSONL inside the
//! owner-only state directory; the remote surface never exposes raw journal
//! contents, only the bounded deterministic resolver projection.
//!
//! This module records *observation* only. Knowledge synthesis, the memory
//! worker, and the vector-backed resolver stay unimplemented (O3/O4); the
//! resolver marks those fields explicitly so readers do not mistake an empty
//! projection for a gap.

pub(crate) mod cli;
pub(crate) mod resolver;
mod store;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config;
use crate::orchestration::{Backend, ControlAction, Operation, TaskRequest};

pub(crate) use store::{JournalStatus, ListFilter, ObservationStore};

/// Record schema version; bump when the stored observation shape changes.
pub(crate) const OBSERVATION_SCHEMA_VERSION: u32 = 1;
/// Bundle schema version for `context_resolve` output.
pub(crate) const CONTEXT_SCHEMA_VERSION: u32 = 1;

/// Preview budget for caller-supplied instruction text. The full text lives
/// only in the authoritative backend receipt; the journal keeps a bounded
/// preview plus a SHA-256 digest for reconciliation.
pub(crate) const INSTRUCTION_PREVIEW_BYTES: usize = 4096;
/// Serialized budget for an inline task-view snapshot inside one observation.
pub(crate) const VIEW_INLINE_BYTES: usize = 8192;
const ERROR_PREVIEW_BYTES: usize = 512;

/// Structured-field keys that may carry caller secrets. Views are sanitized
/// before they are journaled: every entry whose key matches is removed
/// recursively so credential-bearing fields never reach the store or any
/// downstream worker input.
const DENY_KEYS_EXACT: &[&str] = &[
    "task",
    "input",
    "prompt",
    "auth",
    "api_key",
    "apikey",
    "bearer",
    "private_key",
];
const DENY_KEYS_SUBSTRING: &[&str] = &[
    "secret",
    "token",
    "password",
    "passwd",
    "credential",
    "authorization",
];

/// Who carried the normalized instruction across the boundary. `principal`
/// stays empty until a caller identity model exists; today only the
/// transport is authoritative.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ActorRef {
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

impl ActorRef {
    /// The MCP transport label for one `call_tool` dispatch.
    pub(crate) fn mcp(public: bool) -> Self {
        Self {
            transport: if public { "mcp-public" } else { "mcp-stdio" }.to_owned(),
            principal: None,
        }
    }
}

/// The backend that owns the observed operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TargetRef {
    pub backend: String,
}

/// The session instance that produced the record. Observations key on
/// `session_id` only so context survives instance restarts; the instance
/// fields make the producing boundary explicit for forensics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SessionInstanceRef {
    pub started_at: u64,
    pub process_id: u32,
}

/// Bounded, expiring evidence pointers copied from the observed task view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct EvidenceRefRecord {
    pub evidence_id: String,
    pub bytes: usize,
    pub retention_seconds: u64,
}

/// Current-state pointer extracted from an authoritative backend record.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct StateRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

/// Which normalized operation produced this observation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Provenance {
    /// Compatibility tool name (e.g. `codex_task_start`).
    pub tool: String,
    /// Boundary layer that recorded the observation.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_action: Option<String>,
}

/// Closed set of observation kinds. Adding a kind bumps the schema version.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationKind {
    /// A caller instruction reference (bounded preview + digest only).
    Instruction,
    /// A mutating operation was accepted and dispatched.
    OperationAccepted,
    /// A task-state view projected from an authoritative backend record.
    ExecutionState,
    /// A new bounded evidence reference appeared in a task view.
    Evidence,
    /// A capability/verification probe result (backend status).
    Verification,
    /// A caller-visible result was delivered for a mutating operation.
    Delivery,
    /// Reconciliation-relevant signal: dispatch error or explicit
    /// reconciliation-required state.
    Reconciliation,
}

impl ObservationKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ObservationKind::Instruction => "instruction",
            ObservationKind::OperationAccepted => "operation_accepted",
            ObservationKind::ExecutionState => "execution_state",
            ObservationKind::Evidence => "evidence",
            ObservationKind::Verification => "verification",
            ObservationKind::Delivery => "delivery",
            ObservationKind::Reconciliation => "reconciliation",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "instruction" => ObservationKind::Instruction,
            "operation_accepted" => ObservationKind::OperationAccepted,
            "execution_state" => ObservationKind::ExecutionState,
            "evidence" => ObservationKind::Evidence,
            "verification" => ObservationKind::Verification,
            "delivery" => ObservationKind::Delivery,
            "reconciliation" => ObservationKind::Reconciliation,
            _ => return None,
        })
    }
}

/// What the observation carries. Every variant is bounded: full instruction
/// text and unbounded views never enter the journal.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ObservationContent {
    /// Bounded caller-supplied text: preview + SHA-256 digest.
    Text {
        preview: String,
        total_bytes: usize,
        sha256: String,
        truncated: bool,
    },
    /// Sanitized backend task view small enough to inline.
    View { view: Value },
    /// The sanitized view exceeded the inline budget; the digest stays for
    /// reconciliation while the body remains in the authoritative record.
    ViewDigest {
        sha256: String,
        total_bytes: usize,
        truncated: bool,
    },
    /// Bounded caller-visible error text.
    Error { preview: String },
    /// Nothing caller-visible was recorded for this observation.
    None,
}

/// One append-only journal record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Observation {
    pub id: Uuid,
    pub schema_version: u32,
    /// Unix seconds when Temote observed the boundary event.
    pub observed_at: u64,
    /// Unix seconds the operation was accepted, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<u64>,
    pub session_id: String,
    pub session_instance: SessionInstanceRef,
    /// Repository identity label: the session's canonical scope directory
    /// name. `None` when the scope is a filesystem root; the V2 identity
    /// model supersedes this label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Reserved for the V2 per-task workspace identity; unset today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Backend execution identity: thread_id, opencode_session_id,
    /// acp_session_id, or devin_session_id, whichever the view carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub actor: ActorRef,
    pub target: TargetRef,
    /// The normalized action (`task_start`, `task_control`, ...).
    pub action: String,
    pub kind: ObservationKind,
    pub content: ObservationContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_ref: Option<StateRef>,
    #[serde(default)]
    pub evidence_refs: Vec<EvidenceRefRecord>,
    pub provenance: Provenance,
    /// Scope-monotonic journal revision assigned at append time.
    pub revision: u64,
    /// Idempotent append key: re-recording the same semantic event is a no-op.
    pub dedupe_key: String,
}

/// Canonical session instance fields for one observation.
fn instance_of(session: &config::Session) -> SessionInstanceRef {
    SessionInstanceRef {
        started_at: session.started_at,
        process_id: session.process_id,
    }
}

/// Repository label for the session scope; `None` at a filesystem root.
pub(crate) fn repository_label(session: &config::Session) -> Option<String> {
    session
        .cwd
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// UTF-8-safe byte truncation.
fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn text_content(text: &str, preview_bytes: usize) -> ObservationContent {
    let (preview, truncated) = truncate_utf8(text, preview_bytes);
    ObservationContent::Text {
        preview,
        total_bytes: text.len(),
        sha256: sha256_hex(text.as_bytes()),
        truncated,
    }
}

/// Strip keys that may carry caller secrets or instruction text, recursively.
/// `task`/`input` are denied even though a task view should never echo them:
/// the denylist is the boundary defense, not an assumption about backends.
fn sanitize_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut cleaned = Map::with_capacity(map.len());
            for (key, entry) in map {
                let lower = key.to_ascii_lowercase();
                let denied = DENY_KEYS_EXACT.iter().any(|name| lower == *name)
                    || DENY_KEYS_SUBSTRING.iter().any(|name| lower.contains(name));
                if denied {
                    continue;
                }
                cleaned.insert(key.clone(), sanitize_value(entry));
            }
            Value::Object(cleaned)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_value).collect()),
        other => other.clone(),
    }
}

/// Bound a backend result view for journaling: sanitize, then inline when it
/// fits the byte budget, else keep only its digest.
fn view_content(view: &Value) -> (Value, ObservationContent) {
    let sanitized = sanitize_value(view);
    let serialized = serde_json::to_vec(&sanitized).unwrap_or_default();
    if serialized.len() <= VIEW_INLINE_BYTES {
        (
            sanitized.clone(),
            ObservationContent::View { view: sanitized },
        )
    } else {
        (
            sanitized,
            ObservationContent::ViewDigest {
                sha256: sha256_hex(&serialized),
                total_bytes: serialized.len(),
                truncated: true,
            },
        )
    }
}

fn execution_id_of(view: &Value) -> Option<String> {
    for key in [
        "thread_id",
        "opencode_session_id",
        "acp_session_id",
        "devin_session_id",
    ] {
        if let Some(value) = view.get(key).and_then(Value::as_str)
            && !value.is_empty()
        {
            return Some(value.to_owned());
        }
    }
    None
}

fn state_ref_of(view: &Value) -> Option<StateRef> {
    let task_id = view
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let status = view
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if task_id.is_none() && status.is_none() {
        return None;
    }
    Some(StateRef {
        task_id,
        status,
        revision: view.get("revision").and_then(Value::as_u64),
        generation: view.get("generation").and_then(Value::as_u64),
    })
}

fn evidence_refs_of(view: &Value) -> Vec<EvidenceRefRecord> {
    view.get("evidence")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter()
                .filter_map(|entry| {
                    Some(EvidenceRefRecord {
                        evidence_id: entry.get("evidence_id")?.as_str()?.to_owned(),
                        bytes: entry.get("bytes")?.as_u64()? as usize,
                        retention_seconds: entry.get("retention_seconds")?.as_u64()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn reconciliation_marked(view: &Value) -> bool {
    view.get("reconciliation_required").and_then(Value::as_bool) == Some(true)
        || view.get("status").and_then(Value::as_str) == Some("reconciliation_required")
}

fn action_of(operation: Operation) -> &'static str {
    match operation {
        Operation::Status => "status",
        Operation::TaskStart => "task_start",
        Operation::TaskGet => "task_get",
        Operation::TaskList => "task_list",
        Operation::TaskControl => "task_control",
    }
}

/// Build one journal record; revision stays 0 until the store assigns it.
fn base_observation(
    session: &config::Session,
    actor: &ActorRef,
    backend: Backend,
    action: &str,
    tool: String,
    kind: ObservationKind,
    dedupe_key: String,
) -> Observation {
    Observation {
        id: Uuid::new_v4(),
        schema_version: OBSERVATION_SCHEMA_VERSION,
        observed_at: config::unix_time(),
        accepted_at: None,
        session_id: session.id.clone(),
        session_instance: instance_of(session),
        repository: repository_label(session),
        workspace_id: None,
        task_id: None,
        execution_id: None,
        operation_id: None,
        actor: actor.clone(),
        target: TargetRef {
            backend: backend.name().to_owned(),
        },
        action: action.to_owned(),
        kind,
        content: ObservationContent::None,
        state_ref: None,
        evidence_refs: Vec::new(),
        provenance: Provenance {
            tool: tool.to_owned(),
            source: "orchestration".to_owned(),
            control_action: None,
        },
        revision: 0,
        dedupe_key,
    }
}

/// Append one observation; failures are recorded as gaps in the journal
/// sidecar and never fail the operation itself (issue §14).
fn append_best_effort(observation: Observation) {
    let store = match ObservationStore::default_store() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("observation store unavailable: {error:#}");
            return;
        }
    };
    let session_id = observation.session_id.clone();
    if let Err(error) = store.append(observation) {
        store.note_write_failure(&session_id, &error);
        eprintln!("observation append failed: {error:#}");
    }
}

/// Record the normalized instruction before approval and before any backend
/// side effect. A denied or failed dispatch still leaves the instruction in
/// the journal; acceptance is a separate observation.
pub(crate) fn record_instruction(
    session: &config::Session,
    actor: &ActorRef,
    backend: Backend,
    operation: Operation,
    request: &TaskRequest<'_>,
    args: &Value,
) {
    let action = action_of(operation);
    let mut observation = base_observation(
        session,
        actor,
        backend,
        action,
        backend.operation_name(operation),
        ObservationKind::Instruction,
        String::new(),
    );
    let text: Option<&str> = match request {
        TaskRequest::Start(start) => {
            observation.operation_id = Some(start.operation_id.to_owned());
            observation.dedupe_key = format!("instr:{}:start:{}", session.id, start.operation_id);
            args.get("task").and_then(Value::as_str)
        }
        TaskRequest::Control(control) => {
            observation.operation_id = Some(control.operation_id.to_owned());
            observation.task_id = Some(control.task_id.to_owned());
            observation.provenance.control_action = Some(control.action.as_str().to_owned());
            observation.dedupe_key =
                format!("instr:{}:control:{}", session.id, control.operation_id);
            (control.action == ControlAction::Steer)
                .then(|| args.get("input").and_then(Value::as_str))
                .flatten()
        }
        TaskRequest::Get(get) => {
            observation.task_id = Some(get.task_id.to_owned());
            observation.dedupe_key = format!("instr:{}:get:{}", session.id, get.task_id);
            None
        }
        TaskRequest::List => {
            observation.dedupe_key = format!("instr:{}:list", session.id);
            None
        }
        TaskRequest::Status => {
            observation.dedupe_key = format!("instr:{}:status:{}", session.id, backend.name());
            None
        }
    };
    observation.content = match text {
        Some(text) => text_content(text, INSTRUCTION_PREVIEW_BYTES),
        None => ObservationContent::None,
    };
    append_best_effort(observation);
}

/// One state-bearing view to journal: shared by task_get/status replies and
/// by every entry of a task_list projection.
fn record_state_view(
    session: &config::Session,
    actor: &ActorRef,
    backend: Backend,
    operation: Operation,
    view: &Value,
) {
    let Some(state_ref) = state_ref_of(view) else {
        return;
    };
    let action = action_of(operation);
    let tool = backend.operation_name(operation);
    let task_label = state_ref.task_id.clone().unwrap_or_default();
    let (sanitized, content) = view_content(view);
    let mut observation = base_observation(
        session,
        actor,
        backend,
        action,
        tool.clone(),
        ObservationKind::ExecutionState,
        format!(
            "state:{}:{}:{}:{}",
            session.id,
            backend.name(),
            task_label,
            content_digest(&content)
        ),
    );
    observation.task_id = state_ref.task_id.clone();
    observation.state_ref = Some(state_ref);
    observation.execution_id = execution_id_of(&sanitized);
    observation.evidence_refs = evidence_refs_of(&sanitized);
    observation.content = content;
    append_best_effort(observation.clone());

    // Bounded evidence refs crossing the boundary are observations of their
    // own, deduped per record so repeated polls do not re-log them.
    for evidence in &observation.evidence_refs {
        let mut entry = base_observation(
            session,
            actor,
            backend,
            action,
            tool.clone(),
            ObservationKind::Evidence,
            format!("ev:{}:{}", session.id, evidence.evidence_id),
        );
        entry.task_id = observation.task_id.clone();
        entry.evidence_refs = vec![evidence.clone()];
        append_best_effort(entry);
    }

    if reconciliation_marked(view) {
        let mut entry = base_observation(
            session,
            actor,
            backend,
            action,
            tool,
            ObservationKind::Reconciliation,
            format!(
                "recon:{}:{}:{}",
                session.id,
                task_label,
                observation
                    .state_ref
                    .as_ref()
                    .and_then(|state| state.revision)
                    .unwrap_or_default()
            ),
        );
        entry.task_id = observation.task_id;
        entry.state_ref = observation.state_ref;
        append_best_effort(entry);
    }
}

fn content_digest(content: &ObservationContent) -> String {
    match content {
        ObservationContent::View { view } => sha256_hex(view.to_string().as_bytes()),
        ObservationContent::ViewDigest { sha256, .. } => sha256.clone(),
        ObservationContent::Text { sha256, .. } => sha256.clone(),
        ObservationContent::Error { preview } => sha256_hex(preview.as_bytes()),
        ObservationContent::None => "none".to_owned(),
    }
}

/// Record the caller-visible outcome of one dispatched operation.
pub(crate) fn record_outcome(
    session: &config::Session,
    actor: &ActorRef,
    backend: Backend,
    operation: Operation,
    request: &TaskRequest<'_>,
    result: &Result<Value>,
) {
    let action = action_of(operation);
    let tool = backend.operation_name(operation);
    match result {
        Ok(view) => {
            let mutating = matches!(request, TaskRequest::Start(_) | TaskRequest::Control(_));
            if mutating {
                let operation_id = match request {
                    TaskRequest::Start(start) => Some(start.operation_id.to_owned()),
                    TaskRequest::Control(control) => Some(control.operation_id.to_owned()),
                    _ => None,
                };
                for (kind, prefix) in [
                    (ObservationKind::OperationAccepted, "accepted"),
                    (ObservationKind::Delivery, "del"),
                ] {
                    let mut observation = base_observation(
                        session,
                        actor,
                        backend,
                        action,
                        tool.clone(),
                        kind,
                        format!(
                            "{prefix}:{}:{}",
                            session.id,
                            operation_id.as_deref().unwrap_or_default()
                        ),
                    );
                    observation.operation_id = operation_id.clone();
                    observation.accepted_at = Some(observation.observed_at);
                    observation.task_id = view
                        .get("task_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    observation.execution_id = execution_id_of(view);
                    observation.state_ref = state_ref_of(view);
                    if let TaskRequest::Control(control) = request {
                        observation.provenance.control_action =
                            Some(control.action.as_str().to_owned());
                    }
                    append_best_effort(observation);
                }
            }
            match request {
                TaskRequest::Status => {
                    let (_sanitized, content) = view_content(view);
                    let mut observation = base_observation(
                        session,
                        actor,
                        backend,
                        action,
                        tool,
                        ObservationKind::Verification,
                        format!(
                            "verify:{}:{}:{}",
                            session.id,
                            backend.name(),
                            content_digest(&content)
                        ),
                    );
                    observation.content = content;
                    append_best_effort(observation);
                }
                TaskRequest::List => {
                    if let Some(tasks) = view.get("tasks").and_then(Value::as_array) {
                        for task in tasks {
                            record_state_view(session, actor, backend, operation, task);
                        }
                    }
                }
                _ => record_state_view(session, actor, backend, operation, view),
            }
        }
        Err(error) => {
            // The backend-side outcome is unknown; record a reconciliation
            // observation rather than a state claim (issue §14: prefer the
            // authoritative record, expose the gap, never silently replay).
            let (preview, _) = truncate_utf8(&format!("{error:#}"), ERROR_PREVIEW_BYTES);
            let task_id = match request {
                TaskRequest::Get(get) => Some(get.task_id.to_owned()),
                TaskRequest::Control(control) => Some(control.task_id.to_owned()),
                _ => None,
            };
            let operation_id = match request {
                TaskRequest::Start(start) => Some(start.operation_id.to_owned()),
                TaskRequest::Control(control) => Some(control.operation_id.to_owned()),
                _ => None,
            };
            let key_target = task_id
                .clone()
                .or_else(|| operation_id.clone())
                .unwrap_or_else(|| action.to_owned());
            let mut observation = base_observation(
                session,
                actor,
                backend,
                action,
                tool,
                ObservationKind::Reconciliation,
                format!(
                    "err:{}:{}:{}:{}",
                    session.id,
                    action,
                    key_target,
                    sha256_hex(preview.as_bytes())
                ),
            );
            observation.task_id = task_id;
            observation.operation_id = operation_id;
            observation.state_ref = Some(StateRef {
                task_id: observation.task_id.clone(),
                status: Some("dispatch_error".to_owned()),
                ..StateRef::default()
            });
            observation.content = ObservationContent::Error { preview };
            append_best_effort(observation);
        }
    }
}

/// Journal-backed status for `context_status`; `repository` matches the
/// session label so diagnostics stay scoped.
pub(crate) fn session_status(session_id: &str) -> Result<JournalStatus> {
    ObservationStore::default_store()?.status(session_id)
}

/// Bounded metadata projection of one observation for listings: no content
/// body by default (issue §6).
pub(crate) fn listing_view(observation: &Observation, include_content: bool) -> Value {
    let mut view = json!({
        "id": observation.id,
        "revision": observation.revision,
        "kind": observation.kind.as_str(),
        "action": observation.action,
        "observed_at": observation.observed_at,
        "accepted_at": observation.accepted_at,
        "session_id": observation.session_id,
        "repository": observation.repository,
        "task_id": observation.task_id,
        "execution_id": observation.execution_id,
        "operation_id": observation.operation_id,
        "actor": observation.actor,
        "target": observation.target,
        "state_ref": observation.state_ref,
        "evidence_refs": observation.evidence_refs,
        "provenance": observation.provenance,
        "dedupe_key": observation.dedupe_key,
    });
    if include_content {
        view["content"] = serde_json::to_value(&observation.content).unwrap_or(Value::Null);
    } else {
        view["content"] = json!(content_descriptor(&observation.content));
    }
    view
}

fn content_descriptor(content: &ObservationContent) -> Value {
    match content {
        ObservationContent::Text {
            total_bytes,
            sha256,
            truncated,
            ..
        } => json!({
            "kind": "text",
            "total_bytes": total_bytes,
            "sha256": sha256,
            "truncated": truncated,
        }),
        ObservationContent::View { .. } => json!({"kind": "view"}),
        ObservationContent::ViewDigest {
            sha256,
            total_bytes,
            ..
        } => json!({
            "kind": "view_digest",
            "sha256": sha256,
            "total_bytes": total_bytes,
        }),
        ObservationContent::Error { .. } => json!({"kind": "error"}),
        ObservationContent::None => json!({"kind": "none"}),
    }
}

#[cfg(test)]
pub(crate) mod tests_helpers {
    use std::path::PathBuf;
    use uuid::Uuid;

    use crate::config;
    use crate::test_support;

    /// A session rooted at a unique private directory, for resolver tests.
    pub(crate) fn temp_session() -> (config::Session, PathBuf) {
        let root = test_support::private_process_root()
            .unwrap()
            .join("observation-resolve")
            .join(Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(&root).unwrap();
        let session = config::new_session(&root, None, false).unwrap();
        (session, root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn instruction_content_bounds_preview_and_digests() {
        let text = "x".repeat(INSTRUCTION_PREVIEW_BYTES * 3);
        match text_content(&text, INSTRUCTION_PREVIEW_BYTES) {
            ObservationContent::Text {
                preview,
                total_bytes,
                sha256,
                truncated,
            } => {
                assert!(truncated);
                assert_eq!(preview.len(), INSTRUCTION_PREVIEW_BYTES);
                assert_eq!(total_bytes, text.len());
                assert_eq!(sha256, sha256_hex(text.as_bytes()));
            }
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn utf8_preview_never_splits_multibyte_characters() {
        let text = "é".repeat(INSTRUCTION_PREVIEW_BYTES);
        match text_content(&text, INSTRUCTION_PREVIEW_BYTES) {
            ObservationContent::Text {
                preview, truncated, ..
            } => {
                assert!(truncated);
                assert!(preview.len() <= INSTRUCTION_PREVIEW_BYTES);
                assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
            }
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn view_content_strips_secret_bearing_fields_recursively() {
        let view = json!({
            "task_id": "t1",
            "status": "running",
            "task": "the raw instruction must never be journaled",
            "api_key": "sk-secret",
            "nested": {
                "github_token": "tok",
                "password": "pw",
                "keep": "yes",
                "deeper": [{"authorization": "bearer x", "ok": 1}],
            },
            "report": {"rev": 1},
        });
        let (sanitized, _content) = view_content(&view);
        assert_eq!(sanitized["task_id"], json!("t1"));
        assert!(sanitized.get("task").is_none());
        assert!(sanitized.get("api_key").is_none());
        assert!(sanitized["nested"].get("github_token").is_none());
        assert!(sanitized["nested"].get("password").is_none());
        assert_eq!(sanitized["nested"]["keep"], json!("yes"));
        assert!(
            sanitized["nested"]["deeper"][0]
                .get("authorization")
                .is_none()
        );
        assert_eq!(sanitized["nested"]["deeper"][0]["ok"], json!(1));
        assert_eq!(sanitized["report"]["rev"], json!(1));
    }

    #[test]
    fn oversized_view_is_digested_not_inlined() {
        let view = json!({
            "task_id": "t1",
            "status": "running",
            "blob": "y".repeat(VIEW_INLINE_BYTES * 2),
        });
        match view_content(&view).1 {
            ObservationContent::ViewDigest {
                total_bytes,
                truncated,
                ..
            } => {
                assert!(truncated);
                assert!(total_bytes > VIEW_INLINE_BYTES);
            }
            _ => panic!("expected a digest-only view"),
        }
    }

    #[tokio::test]
    async fn boundary_records_dedupe_on_semantic_retry() {
        let (session, _root) = tests_helpers::temp_session();
        let actor = ActorRef::mcp(false);
        let task_id = Uuid::new_v4().to_string();
        let backend = crate::orchestration::Backend::Codex;
        let operation = crate::orchestration::Operation::TaskGet;
        let args = json!({"task_id": task_id});
        let result: anyhow::Result<Value> = Err(anyhow::anyhow!("unknown task"));
        for _ in 0..2 {
            let request =
                crate::orchestration::TaskRequest::parse(backend, operation, &args).unwrap();
            record_instruction(&session, &actor, backend, operation, &request, &args);
            record_outcome(&session, &actor, backend, operation, &request, &result);
        }
        let store = ObservationStore::default_store().unwrap();
        let (records, corrupt) = store.list(&session.id, &ListFilter::default()).unwrap();
        assert_eq!(corrupt, 0);
        let instructions: Vec<_> = records
            .iter()
            .filter(|record| record.kind == ObservationKind::Instruction)
            .collect();
        let reconciliations: Vec<_> = records
            .iter()
            .filter(|record| record.kind == ObservationKind::Reconciliation)
            .collect();
        // Same semantic operation retried: one instruction, one identical
        // reconciliation observation — no duplicate proliferation.
        assert_eq!(instructions.len(), 1);
        assert_eq!(reconciliations.len(), 1);
        let instruction = instructions[0];
        assert_eq!(instruction.task_id.as_deref(), Some(task_id.as_str()));
        assert_eq!(instruction.actor.transport, "mcp-stdio");
        assert_eq!(instruction.action, "task_get");
        assert_eq!(instruction.target.backend, "codex");
        assert!(instruction.revision >= 1);
        let error = &reconciliations[0];
        assert_eq!(error.task_id.as_deref(), Some(task_id.as_str()));
        assert_eq!(
            error.state_ref.as_ref().and_then(|s| s.status.as_deref()),
            Some("dispatch_error")
        );
        assert!(error.revision > instruction.revision);
    }

    #[tokio::test]
    async fn same_semantic_operation_shapes_identically_across_transports() {
        let (session, _root) = tests_helpers::temp_session();
        let task_id = Uuid::new_v4().to_string();
        let backend = crate::orchestration::Backend::Codex;
        let operation = crate::orchestration::Operation::TaskGet;
        let args = json!({"task_id": task_id});
        let result: anyhow::Result<Value> = Err(anyhow::anyhow!("unknown task"));
        for public in [false, true] {
            let actor = ActorRef::mcp(public);
            let request =
                crate::orchestration::TaskRequest::parse(backend, operation, &args).unwrap();
            record_instruction(&session, &actor, backend, operation, &request, &args);
            record_outcome(&session, &actor, backend, operation, &request, &result);
        }
        let store = ObservationStore::default_store().unwrap();
        let (records, _) = store.list(&session.id, &ListFilter::default()).unwrap();
        let instruction = records
            .iter()
            .find(|record| record.kind == ObservationKind::Instruction)
            .unwrap();
        // The shape is transport-independent; only actor.transport differs
        // and the retried record deduped away entirely.
        assert_eq!(instruction.schema_version, OBSERVATION_SCHEMA_VERSION);
        assert_eq!(instruction.action, "task_get");
        assert_eq!(instruction.kind.as_str(), "instruction");
        assert!(serde_json::to_value(instruction).unwrap().is_object());
    }
}
