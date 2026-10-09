//! Best-effort observation of Codex app-server v2 `item/started` user messages.
//! This producer sees only conversations owned by Temote's app-server client.

use serde::Deserialize;
use serde_json::Value;

use crate::config;
use crate::prompt_ingress::{self, Agent, Envelope, GapReason, Kind, SessionFence};

const INGRESS_SCHEMA_VERSION: u32 = 1;
const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemStartedV2 {
    thread_id: String,
    turn_id: String,
    started_at_ms: i64,
    item: Value,
}

#[derive(Deserialize)]
struct UserMessageV2 {
    id: String,
    content: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub(crate) body: Option<String>,
}

/// The generated Codex 0.157.1 v2 ServerNotification schema requires all
/// three IDs, startedAtMs, and a userMessage item with content. Unknown item
/// variants, including hookPrompt, never become user observations.
pub(crate) fn parse_item_started(method: &str, params: Option<&Value>) -> Option<Candidate> {
    if method != "item/started" {
        return None;
    }
    let params = params?;
    if params.pointer("/item/type").and_then(Value::as_str) != Some("userMessage") {
        return None;
    }
    let started: ItemStartedV2 = serde_json::from_value(params.clone()).ok()?;
    if started.started_at_ms < 0 {
        return None;
    }
    let message: UserMessageV2 = serde_json::from_value(started.item).ok()?;
    if started.thread_id.is_empty() || started.turn_id.is_empty() || message.id.is_empty() {
        return None;
    }
    let body = match message.content.as_slice() {
        [input] if input.get("type").and_then(Value::as_str) == Some("text") => input
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty() && text.len() <= MAX_BODY_BYTES)
            .map(str::to_owned),
        _ => None,
    };
    Some(Candidate {
        thread_id: started.thread_id,
        turn_id: started.turn_id,
        item_id: message.id,
        body,
    })
}

pub(crate) struct Binding {
    pub session: config::Session,
    pub task_id: String,
    pub execution_id: String,
}

/// The caller obtains Binding from a retained task record only after exact
/// owner, canonical scope, thread, turn, and generation checks. An observation
/// error is returned only to the observer task, never to delegated execution.
pub(crate) async fn observe(candidate: Candidate, binding: Binding) -> anyhow::Result<()> {
    let envelope = Envelope {
        schema_version: INGRESS_SCHEMA_VERSION,
        host_id: crate::host_identity::resolve()?,
        session: Some(SessionFence {
            session_id: binding.session.id,
            started_at: binding.session.started_at,
            process_id: binding.session.process_id,
            scope_cwd: binding.session.cwd,
        }),
        agent: Agent::Codex,
        conversation_id: candidate.thread_id,
        source_event_id: candidate.item_id,
        kind: if candidate.body.is_some() {
            Kind::UserPromptAccepted
        } else {
            Kind::PromptObservationGap
        },
        gap_reason: candidate
            .body
            .is_none()
            .then_some(GapReason::UnsupportedContent),
        repository_key: None,
        workspace_id: None,
        task_id: Some(binding.task_id),
        execution_id: Some(binding.execution_id),
        agent_turn_id: Some(candidate.turn_id),
    };
    prompt_ingress::Store::default_store()?
        .observe_owned(envelope, candidate.body.as_deref())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(content: Value) -> Value {
        json!({
            "threadId": "thread-1", "turnId": "turn-1", "startedAtMs": 42,
            "item": {"type": "userMessage", "id": "item-1", "content": content}
        })
    }

    #[test]
    fn v2_user_message_fixture_preserves_source_item_and_text() {
        let candidate = parse_item_started(
            "item/started",
            Some(&fixture(
                json!([{"type": "text", "text": "user instruction"}]),
            )),
        )
        .unwrap();
        assert_eq!(candidate.thread_id, "thread-1");
        assert_eq!(candidate.turn_id, "turn-1");
        assert_eq!(candidate.item_id, "item-1");
        assert_eq!(candidate.body.as_deref(), Some("user instruction"));
    }

    #[test]
    fn v2_non_user_and_pre_submit_events_are_excluded() {
        let user = fixture(json!([{"type": "text", "text": "private"}]));
        assert!(parse_item_started("turn/started", Some(&user)).is_none());
        assert!(parse_item_started("UserPromptSubmit", Some(&user)).is_none());
        let hook = json!({"threadId":"thread-1", "turnId":"turn-1", "startedAtMs":42,
            "item":{"type":"hookPrompt", "id":"item-2", "fragments":[]}});
        assert!(parse_item_started("item/started", Some(&hook)).is_none());
        let malformed = json!({"threadId":"thread-1", "turnId":"turn-1",
            "item":{"type":"userMessage", "id":"item-1", "content":[]}});
        assert!(parse_item_started("item/started", Some(&malformed)).is_none());
    }

    #[test]
    fn unsupported_user_content_is_a_gap_without_partial_text() {
        let candidate = parse_item_started(
            "item/started",
            Some(&fixture(json!([
                {"type": "text", "text": "partial"},
                {"type": "image", "url": "data:image/png;base64,AAAA"}
            ]))),
        )
        .unwrap();
        assert_eq!(candidate.item_id, "item-1");
        assert_eq!(candidate.body, None);
    }
}
