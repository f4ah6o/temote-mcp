//! Deterministic Context Resolver (issue packet O2).
//!
//! Projects the session-owned observation journal into a bounded context
//! bundle. Resolution is pure projection over journaled observation records:
//! no model calls, no live backend reconcile, deterministic ordering. Missing
//! scopes, corrupt records, and stale revisions are surfaced in `partial` /
//! `freshness` rather than hidden (issue §14).

use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use crate::config;
#[cfg(unix)]
use crate::prompt_ingress::{self, Kind as PromptKind};

use super::{
    CONTEXT_SCHEMA_VERSION, JournalStatus, ListFilter, Observation, ObservationContent,
    ObservationKind, ObservationStore, repository_label,
};

const DEFAULT_TASK_LIMIT: usize = 16;
const MAX_TASK_LIMIT: usize = 64;
const MAX_REFS: usize = 64;
const MAX_REFS_PER_TASK: usize = 8;
const MAX_QUERY_BYTES: usize = 512;
const MAX_REPOSITORY_BYTES: usize = 256;

#[cfg(unix)]
fn prompt_context_projection(session: &config::Session) -> Value {
    let (prompt_records, degraded) =
        match prompt_ingress::Store::default_store().and_then(|store| store.for_session(session)) {
            Ok(records) => (records, false),
            Err(_) => (Vec::new(), true),
        };
    let prompt_refs: Vec<Value> = prompt_records
        .iter()
        .rev()
        .take(16)
        .map(|record| {
            json!({
                "observation_id": record.id,
                "agent": record.envelope.agent,
                "kind": record.envelope.kind,
                "gap_reason": record.envelope.gap_reason,
                "observed_at": record.observed_at,
                "body_sha256": record.body_sha256,
                "task_id": record.envelope.task_id,
                "execution_id": record.envelope.execution_id,
                "agent_turn_id": record.envelope.agent_turn_id,
                "correlation_links": record.correlations.iter().take(8).map(|link| json!({
                    "task_id": link.task_id, "execution_id": link.execution_id,
                    "agent_turn_id": link.agent_turn_id,
                    "authority": "canonical_backend_task_identity",
                })).collect::<Vec<_>>(),
                "provenance": "owner_local_source_asserted",
                "correlation": if !record.correlations.is_empty() { "exact_backend_task" }
                    else if record.envelope.task_id.is_some() { "explicit" } else { "unbound" },
            })
        })
        .collect();
    let latest_user_intent = prompt_records
        .iter()
        .rev()
        .find(|record| record.envelope.kind == PromptKind::UserPromptAccepted)
        .map(|record| {
            json!({
                "observation_id": record.id,
                "observed_at": record.observed_at,
                "agent": record.envelope.agent,
                "body_sha256": record.body_sha256,
                "provenance": "direct_agent_local_ingress",
                "content": "owner_local_only",
            })
        });
    let recent_user_steers: Vec<Value> = prompt_refs
        .iter()
        .filter(|value| value["kind"] == "user_steer_accepted")
        .take(8)
        .cloned()
        .collect();
    let mut seen_conversations = BTreeSet::new();
    let mut active_agent_conversations = Vec::new();
    let mut prompt_conversations = Vec::new();
    for record in prompt_records.iter().rev() {
        let key = (
            format!("{:?}", record.envelope.agent),
            record.envelope.conversation_id.clone(),
        );
        if !seen_conversations.insert(key) {
            continue;
        }
        if prompt_conversations.len() >= 8 {
            break;
        }
        let gap = prompt_records.iter().rev().find(|other| {
            other.envelope.agent == record.envelope.agent
                && other.envelope.conversation_id == record.envelope.conversation_id
                && other.envelope.kind == PromptKind::PromptObservationGap
        });
        active_agent_conversations.push(json!({
            "agent": record.envelope.agent,
            "conversation_id": record.envelope.conversation_id,
            "last_observed_at": record.observed_at,
        }));
        prompt_conversations.push(json!({
            "agent": record.envelope.agent,
            "conversation_id": record.envelope.conversation_id,
            "coverage": "partial",
            "reason": "observed events do not prove complete direct-conversation history",
            "last_observed_at": record.observed_at,
            "last_source_event_id": record.envelope.source_event_id,
            "gap_detected": gap.is_some(),
            "gap_reason": gap.and_then(|event| event.envelope.gap_reason),
        }));
    }
    let last = prompt_records.last().map(|record| record.observed_at);
    let age = last.map(|time| config::unix_time().saturating_sub(time));
    let worker_refs = prompt_ingress::worker_eligible(
        &prompt_records,
        config::unix_time(),
        prompt_ingress::WorkerPolicy {
            max_age_seconds: 86_400,
            max_items: 8,
        },
    )
    .unwrap_or_default();
    json!({
        "latest_user_intent": latest_user_intent,
        "recent_user_steers": recent_user_steers,
        "active_agent_conversations": active_agent_conversations,
        "prompt_refs": prompt_refs,
        "prompt_worker_eligible_refs": worker_refs,
        "prompt_observation_coverage": prompt_ingress::installed_capabilities(),
        "prompt_conversation_coverage": prompt_conversations,
        "prompt_observation_freshness": {
            "last_observed_at": last, "age_seconds": age,
            "stale": age.is_none_or(|age| age > 86_400),
            "degraded": degraded,
            "session_instance": {"started_at": session.started_at, "process_id": session.process_id},
        },
    })
}

#[cfg(not(unix))]
fn prompt_context_projection(_session: &config::Session) -> Value {
    json!({
        "latest_user_intent": null,
        "recent_user_steers": [],
        "active_agent_conversations": [],
        "prompt_refs": [],
        "prompt_worker_eligible_refs": [],
        "prompt_observation_coverage": [{"agent": "codex", "coverage": "unavailable", "reason": "local Unix prompt ingress is unavailable on this platform"},
            {"agent": "devin_acp", "coverage": "unavailable", "reason": "local Unix prompt ingress is unavailable on this platform"},
            {"agent": "devin_cloud", "coverage": "unavailable", "reason": "local Unix prompt ingress is unavailable on this platform"}],
        "prompt_conversation_coverage": [],
        "prompt_observation_freshness": {"last_observed_at": null, "stale": true, "degraded": false},
    })
}

/// Terminal task states on the shared contract.
fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "interrupted" | "failed")
}

/// Task states worth flagging to a fresh head as needing attention.
fn needs_attention(status: &str) -> bool {
    matches!(
        status,
        "waiting_approval"
            | "retryable_failed"
            | "reconciliation_required"
            | "unknown"
            | "dispatch_error"
    )
}

/// A stable reference to the observation that supports a bundle item.
fn support_ref(observation: &Observation) -> Value {
    json!({
        "observation_id": observation.id,
        "revision": observation.revision,
        "kind": observation.kind.as_str(),
    })
}

fn text_descriptor(observation: &Observation) -> Value {
    match &observation.content {
        ObservationContent::Text {
            preview,
            total_bytes,
            sha256,
            truncated,
        } => json!({
            "preview": preview,
            "total_bytes": total_bytes,
            "sha256": sha256,
            "truncated": truncated,
        }),
        _ => Value::Null,
    }
}

/// All observations of one task, rolled up deterministically.
#[derive(Default)]
struct TaskRollup {
    backend: Option<String>,
    instruction: Option<Observation>,
    accepted: Option<Observation>,
    state: Option<Observation>,
    last_error: Option<Observation>,
    evidence: Vec<Observation>,
    reconciliation: Option<Observation>,
    refs: Vec<Value>,
    last_revision: u64,
    last_observed_at: u64,
    backends: BTreeSet<String>,
}

impl TaskRollup {
    fn note(&mut self, observation: &Observation) {
        self.last_revision = self.last_revision.max(observation.revision);
        self.last_observed_at = self.last_observed_at.max(observation.observed_at);
        self.backends.insert(observation.target.backend.clone());
        if self.refs.len() < MAX_REFS_PER_TASK {
            self.refs.push(support_ref(observation));
        }
        match observation.kind {
            ObservationKind::Instruction => {
                // Keep the earliest instruction: the task's origin reference.
                if self.instruction.is_none() {
                    self.instruction = Some(observation.clone());
                }
            }
            ObservationKind::OperationAccepted => {
                if self
                    .accepted
                    .as_ref()
                    .is_none_or(|accepted| observation.revision > accepted.revision)
                {
                    self.accepted = Some(observation.clone());
                }
            }
            ObservationKind::ExecutionState => {
                if self
                    .state
                    .as_ref()
                    .is_none_or(|state| observation.revision > state.revision)
                {
                    self.state = Some(observation.clone());
                }
            }
            ObservationKind::Evidence => self.evidence.push(observation.clone()),
            ObservationKind::Reconciliation => {
                if observation
                    .state_ref
                    .as_ref()
                    .and_then(|state| state.status.as_deref())
                    == Some("dispatch_error")
                {
                    self.last_error = Some(observation.clone());
                } else {
                    self.reconciliation = Some(observation.clone());
                }
            }
            ObservationKind::Verification | ObservationKind::Delivery => {}
        }
    }

    fn status(&self) -> &str {
        self.state
            .as_ref()
            .and_then(|state| state.state_ref.as_ref())
            .and_then(|state| state.status.as_deref())
            .unwrap_or("unobserved")
    }

    fn view(&self) -> Value {
        json!({
            "backend": self.backend,
            "instruction": self.instruction.as_ref().map(|observation| json!({
                "revision": observation.revision,
                "observed_at": observation.observed_at,
                "operation_id": observation.operation_id,
                "actor": observation.actor,
                "content": text_descriptor(observation),
            })),
            "state": self.state.as_ref().map(|observation| json!({
                "revision": observation.revision,
                "observed_at": observation.observed_at,
                "status": self.status(),
                "execution_id": observation.execution_id,
                "state_ref": observation.state_ref,
                "reconciliation_required": self.reconciliation.is_some(),
            })),
            "last_error": self.last_error.as_ref().map(|observation| json!({
                "revision": observation.revision,
                "observed_at": observation.observed_at,
                "content": match &observation.content {
                    ObservationContent::Error { preview } => json!({"preview": preview}),
                    _ => Value::Null,
                },
            })),
            "evidence_refs": self
                .state
                .as_ref()
                .map(|state| state.evidence_refs.clone())
                .unwrap_or_default(),
            "refs": self.refs,
            "last_observed_at": self.last_observed_at,
        })
    }
}

struct ResolveArgs {
    task_id: Option<String>,
    repository: Option<String>,
    query: Option<String>,
    limit: usize,
    at_least_revision: Option<u64>,
}

fn resolve_args(args: &Value) -> Result<ResolveArgs> {
    let task_id = args
        .get("task_id")
        .or_else(|| args.get("task"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(task) = &task_id {
        anyhow::ensure!(task.len() <= 256, "task_id is too long");
    }
    let repository = args
        .get("repository")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(repository) = &repository {
        anyhow::ensure!(
            repository.len() <= MAX_REPOSITORY_BYTES,
            "repository filter is too long"
        );
    }
    let query = args.get("query").and_then(Value::as_str).map(str::to_owned);
    if let Some(query) = &query {
        anyhow::ensure!(query.len() <= MAX_QUERY_BYTES, "query is too long");
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_TASK_LIMIT);
    anyhow::ensure!(
        (1..=MAX_TASK_LIMIT).contains(&limit),
        "limit must be in 1..={MAX_TASK_LIMIT}"
    );
    let at_least_revision = args.get("at_least_revision").and_then(Value::as_u64);
    Ok(ResolveArgs {
        task_id,
        repository,
        query,
        limit,
        at_least_revision,
    })
}

/// Deterministic bundle for `context_resolve`. Session-bound: callers only
/// ever resolve the journal of the session they name.
pub(crate) fn resolve(session: &config::Session, args: &Value) -> Result<Value> {
    let filters = resolve_args(args)?;
    let store = ObservationStore::default_store()?;
    let status = store.status(&session.id)?;
    let (records, corrupt) = store.list(
        &session.id,
        &ListFilter {
            ..ListFilter::default()
        },
    )?;

    let repository = repository_label(session);
    let mut notes: Vec<String> = Vec::new();
    let prompt = prompt_context_projection(session);
    if prompt["prompt_observation_freshness"]["degraded"] == true {
        notes.push("owner-local prompt observations could not be read".to_owned());
    }
    if !status.exists {
        notes.push("no observation journal exists for this session yet".to_owned());
    }
    if corrupt > 0 {
        notes.push(format!("{corrupt} journal line(s) could not be parsed"));
    }

    // Pass 1: every record mentioning this session's repository stays; the
    // operation→task link lets pre-acceptance instructions join their task.
    let mut operation_task: BTreeMap<String, String> = BTreeMap::new();
    for record in &records {
        if let (Some(operation), Some(task)) = (&record.operation_id, &record.task_id) {
            operation_task
                .entry(operation.clone())
                .or_insert_with(|| task.clone());
        }
    }
    let task_key = |record: &Observation| -> Option<String> {
        record.task_id.clone().or_else(|| {
            record
                .operation_id
                .as_ref()
                .and_then(|operation| operation_task.get(operation).cloned())
        })
    };

    let query_lower = filters.query.as_deref().map(str::to_ascii_lowercase);
    let matches_query = |record: &Observation| -> bool {
        let Some(query) = &query_lower else {
            return true;
        };
        let mut haystack = format!(
            "{} {} {} {}",
            record.target.backend,
            record.action,
            record.task_id.as_deref().unwrap_or_default(),
            record
                .state_ref
                .as_ref()
                .and_then(|state| state.status.as_deref())
                .unwrap_or_default(),
        );
        if let ObservationContent::Text { preview, .. } = &record.content {
            haystack.push(' ');
            haystack.push_str(preview);
        }
        haystack.to_ascii_lowercase().contains(query.as_str())
    };

    let mut tasks: BTreeMap<String, TaskRollup> = BTreeMap::new();
    let mut instruction_count = 0usize;
    let mut matched_any = false;
    for record in &records {
        if filters
            .repository
            .as_deref()
            .is_some_and(|wanted| record.repository.as_deref() != Some(wanted))
        {
            continue;
        }
        if !matches_query(record) {
            continue;
        }
        matched_any = true;
        if record.kind == ObservationKind::Instruction {
            instruction_count += 1;
        }
        let Some(key) = task_key(record) else {
            continue;
        };
        if filters
            .task_id
            .as_deref()
            .is_some_and(|wanted| key != wanted)
        {
            continue;
        }
        let rollup = tasks.entry(key).or_default();
        rollup
            .backend
            .get_or_insert_with(|| record.target.backend.clone());
        rollup.note(record);
    }
    if !matched_any && !records.is_empty() {
        notes.push("filters matched no observations".to_owned());
    }

    let mut rollups: Vec<(String, TaskRollup)> = tasks.into_iter().collect();
    rollups.sort_by(|a, b| {
        b.1.last_revision
            .cmp(&a.1.last_revision)
            .then_with(|| a.0.cmp(&b.0))
    });
    rollups.truncate(filters.limit);

    let mut tasks_active = 0usize;
    let mut tasks_terminal = 0usize;
    let mut unresolved: Vec<Value> = Vec::new();
    let mut related: Vec<Value> = Vec::new();
    let mut refs: Vec<Value> = Vec::new();
    for (task_id, rollup) in &rollups {
        let status_text = rollup.status();
        if is_terminal(status_text) {
            tasks_terminal += 1;
        } else {
            tasks_active += 1;
        }
        if needs_attention(status_text) || status_text == "unobserved" {
            unresolved.push(json!({
                "task_id": task_id,
                "status": status_text,
                "reason": if status_text == "unobserved" {
                    "instruction or acceptance recorded without a state view"
                } else {
                    "latest observed state needs attention"
                },
                "refs": rollup
                    .state
                    .as_ref()
                    .or(rollup.accepted.as_ref())
                    .map(|observation| vec![support_ref(observation)])
                    .unwrap_or_default(),
            }));
        }
        if let Some(error) = &rollup.last_error
            && !is_terminal(status_text)
        {
            unresolved.push(json!({
                "task_id": task_id,
                "status": "dispatch_error",
                "reason": "a dispatch returned an error; prefer the authoritative backend record before retrying",
                "refs": [support_ref(error)],
            }));
        }
        let mut entry = rollup.view();
        entry["task_id"] = json!(task_id);
        related.push(entry);
        for reference in &rollup.refs {
            if refs.len() < MAX_REFS {
                refs.push(reference.clone());
            }
        }
    }
    // Deterministic ordering for every emitted list.
    unresolved.sort_by(|a, b| {
        a["task_id"]
            .as_str()
            .cmp(&b["task_id"].as_str())
            .then_with(|| a["status"].as_str().cmp(&b["status"].as_str()))
    });

    let last_observed_at = records.iter().map(|record| record.observed_at).max();
    let backends: Vec<String> = rollups
        .iter()
        .flat_map(|(_, rollup)| rollup.backends.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let stale = filters
        .at_least_revision
        .is_some_and(|required| status.revision < required);
    if stale {
        notes.push(format!(
            "journal revision {} is below the requested {}",
            status.revision,
            filters.at_least_revision.unwrap_or_default()
        ));
    }

    Ok(json!({
        "context_schema_version": CONTEXT_SCHEMA_VERSION,
        "session_id": session.id,
        "workspace": {
            "cwd": session.cwd,
            "repository": repository,
        },
        "current_summary": {
            "observations": status.observations,
            "journal_revision": status.revision,
            "instructions": instruction_count,
            "tasks_total": rollups.len(),
            "tasks_active": tasks_active,
            "tasks_terminal": tasks_terminal,
            "tasks_attention": unresolved.len(),
            "backends": backends,
            "last_observed_at": last_observed_at,
        },
        // Knowledge-plane fields stay empty until the O3 memory worker exists;
        // the marker keeps callers from mistaking "none" for "uncomputed".
        "relevant_decisions": [],
        "relevant_facts": [],
        "constraints": [],
        "known_failure_patterns": [],
        "memory": {
            "worker": "not_implemented",
            "stale": true,
            "note": "O3 memory worker is not implemented; knowledge fields are empty by construction.",
        },
        "unresolved": unresolved,
        "recent_related_tasks": related,
        "refs": refs,
        "latest_user_intent": prompt["latest_user_intent"],
        "recent_user_steers": prompt["recent_user_steers"],
        "active_agent_conversations": prompt["active_agent_conversations"],
        "prompt_refs": prompt["prompt_refs"],
        "prompt_worker_eligible_refs": prompt["prompt_worker_eligible_refs"],
        "prompt_observation_coverage": prompt["prompt_observation_coverage"],
        "prompt_conversation_coverage": prompt["prompt_conversation_coverage"],
        "prompt_observation_freshness": prompt["prompt_observation_freshness"],
        "freshness": {
            "resolved_revision": status.revision,
            "at_least_revision": filters.at_least_revision,
            "stale": stale,
        },
        "partial": {
            "journal_exists": status.exists,
            "journal_degraded": status.degraded,
            "corrupt_lines": corrupt,
            "write_failures": status.write_failures,
            "notes": notes,
        },
        "deterministic": true,
        "generated_at": config::unix_time(),
    }))
}

/// `context_status`: journal counters plus the memory-worker gap marker.
pub(crate) fn status(session: &config::Session, _args: &Value) -> Result<Value> {
    let store = ObservationStore::default_store()?;
    let status: JournalStatus = store.status(&session.id)?;
    Ok(json!({
        "session_id": session.id,
        "workspace": {
            "cwd": session.cwd,
            "repository": repository_label(session),
        },
        "journal": status,
        "memory": {
            "worker": "not_implemented",
            "stale": true,
            "note": "O3 memory worker is not implemented.",
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests_helpers;
    use super::*;
    use crate::observation::{ActorRef, ObservationStore, SessionInstanceRef, TargetRef};

    fn record(
        session_id: &str,
        kind: ObservationKind,
        revision_key: &str,
        task_id: Option<&str>,
        status: Option<&str>,
    ) -> Observation {
        Observation {
            id: uuid::Uuid::new_v4(),
            schema_version: super::super::OBSERVATION_SCHEMA_VERSION,
            observed_at: 10,
            accepted_at: None,
            session_id: session_id.to_owned(),
            session_instance: SessionInstanceRef {
                started_at: 0,
                process_id: 0,
            },
            repository: Some("repo".to_owned()),
            repository_key: Some("github:owner/repo".to_owned()),
            workspace_id: None,
            task_id: task_id.map(str::to_owned),
            execution_id: None,
            operation_id: None,
            actor: ActorRef {
                transport: "test".to_owned(),
                principal: None,
            },
            target: TargetRef {
                backend: "codex".to_owned(),
            },
            action: "task_get".to_owned(),
            kind,
            content: ObservationContent::None,
            state_ref: status.map(|status| super::super::StateRef {
                task_id: task_id.map(str::to_owned),
                status: Some(status.to_owned()),
                revision: Some(1),
                generation: Some(1),
            }),
            evidence_refs: Vec::new(),
            provenance: super::super::Provenance {
                tool: "codex_task_get".to_owned(),
                source: "orchestration".to_owned(),
                control_action: None,
            },
            revision: 0,
            dedupe_key: revision_key.to_owned(),
        }
    }

    #[test]
    fn resolve_projects_task_rollup() {
        let (session, _root) = tests_helpers::temp_session();
        let store = ObservationStore::default_store().unwrap();
        let sid = session.id.clone();
        store
            .append(record(&sid, ObservationKind::Instruction, "i1", None, None))
            .unwrap();
        store
            .append(record(
                &sid,
                ObservationKind::ExecutionState,
                "s1",
                Some("task-a"),
                Some("running"),
            ))
            .unwrap();
        let result = resolve(&session, &json!({})).unwrap();
        assert_eq!(result["current_summary"]["observations"], json!(2));
        assert_eq!(result["current_summary"]["tasks_total"], json!(1));
        assert_eq!(result["current_summary"]["tasks_active"], json!(1));
        assert_eq!(result["unresolved"].as_array().unwrap().len(), 0);
        assert_eq!(
            result["recent_related_tasks"][0]["state"]["status"],
            json!("running")
        );
        assert_eq!(result["partial"]["corrupt_lines"], json!(0));
        assert_eq!(result["memory"]["worker"], json!("not_implemented"));
    }
}
