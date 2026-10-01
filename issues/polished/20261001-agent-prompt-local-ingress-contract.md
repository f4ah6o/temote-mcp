# O1P1: local agent-prompt observation ingress contract + durable append

Status: ready
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
Parent: `issues/open/20261001-agent-prompt-observation-ingress.md`
Scope: parent P0 + P1 only; no Codex/Devin hook in this packet

## 1. Goal

Add the backend-neutral local ingress contract that can durably record a user-visible agent prompt/steer observation exactly once. The packet establishes storage, idempotency, content-reference policy, and read-safe metadata before any Codex or Devin integration begins emitting events.

## 2. Event kinds

Support the parent contract's structural kinds:

```text
user_prompt_accepted
user_steer_accepted
conversation_bound
conversation_unbound
prompt_observation_gap
```

Do not capture hidden reasoning, chain-of-thought, system prompts, or developer prompts.

## 3. Identity / idempotency

The ingress key is:

```text
agent + agent_conversation_id + source_event_id
```

Requirements:

- duplicate delivery is a no-op/replay, never a second observation;
- if an integration lacks a stable source event id, that integration must persist one before calling this ingress (hook-specific work is later);
- an observation may be unbound to Temote task/execution at append time;
- later task/execution correlation is additive linkage and does not rewrite the original raw observation.

## 4. Content boundary

- structural envelope is bounded and indexable;
- raw prompt body is owner-only local content by default;
- ordinary MCP/HTTP/Fabric projections do not return the raw body;
- Fabric replication receives structural metadata/digest only unless a future explicit opt-in contract says otherwise;
- reuse the existing observation content-reference model rather than copying prompt text into multiple records/logs;
- free-text secret scanning is not an authorization boundary.

## 5. Minimum durable fields

Persist a versioned representation covering:

- observation id / schema version / observed_at
- host id
- optional Temote session id
- agent kind
- bounded agent conversation id
- bounded source event id
- prompt observation kind
- optional repository/workspace/task/execution linkage
- owner-only content reference
- provenance needed to distinguish direct-agent prompt capture from ordinary Temote instruction capture

Reject unbounded/invalid identifiers and unknown event kinds.

## 6. Storage / API behavior

- append-only owner-local storage;
- Accepted/duplicate behavior is deterministic across process restart;
- no coding-agent execution is started by this ingress;
- no remote/backend call is performed by this ingress;
- ordinary observation/context readers may see safe structural metadata but not the raw body;
- storage failure does not mutate Task/Execution state.

The exact internal Rust function/tool shape may follow the existing observation store; do not add a public raw-prompt retrieval tool in this packet.

## 7. Acceptance

- [ ] first append creates one durable observation and owner-only content reference.
- [ ] same `agent + conversation + source_event` replay creates no duplicate.
- [ ] same source identity with conflicting immutable envelope fails closed.
- [ ] restart + replay preserves idempotency.
- [ ] unbound prompt append is accepted without fabricating task/execution identity.
- [ ] raw prompt body is absent from ordinary MCP/HTTP/Fabric-safe projections and logs.
- [ ] system/developer/hidden-reasoning event kinds are not accepted through the user-prompt contract.
- [ ] storage failure does not change task/backend execution state.
- [ ] legacy observation records remain readable.

## 8. Tests

- [ ] duplicate/replay + conflict tests
- [ ] process/store reopen idempotency test
- [ ] owner-only content/body non-leak test
- [ ] bounded identifier/content test
- [ ] unbound observation test
- [ ] `cargo fmt --all -- --check` PASS
- [ ] focused observation tests PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## 9. Deferred

Codex hook, Devin ACP/Cloud hook, late Task/Execution correlation, context coverage projection, worker eligibility, and cross-agent live acceptance remain parent P2-P7.
