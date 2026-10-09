# O1P1: local agent-prompt observation ingress contract + durable append

Status: done
Model: unknown
Created: 2026-10-01
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Add backend-neutral, idempotent, durable local ingress for user-visible agent prompt and steer observations.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Add backend-neutral, idempotent, durable local ingress for user-visible agent prompt and steer observations.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 9. Deferred

Codex hook, Devin ACP/Cloud hook, late Task/Execution correlation, context coverage projection, worker eligibility, and cross-agent live acceptance remain parent P2-P7.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Fixed design from source

**2. Event kinds**

Support the parent contract's structural kinds:

```text
user_prompt_accepted
user_steer_accepted
conversation_bound
conversation_unbound
prompt_observation_gap
```

Do not capture hidden reasoning, chain-of-thought, system prompts, or developer prompts.

**3. Identity / idempotency**

The ingress key is:

```text
agent + agent_conversation_id + source_event_id
```

Requirements:

- duplicate delivery is a no-op/replay, never a second observation;
- if an integration lacks a stable source event id, that integration must persist one before calling this ingress (hook-specific work is later);
- an observation may be unbound to Temote task/execution at append time;
- later task/execution correlation is additive linkage and does not rewrite the original raw observation.

**4. Content boundary**

- structural envelope is bounded and indexable;
- raw prompt body is owner-only local content by default;
- ordinary MCP/HTTP/Fabric projections do not return the raw body;
- Fabric replication receives structural metadata/digest only unless a future explicit opt-in contract says otherwise;
- reuse the existing observation content-reference model rather than copying prompt text into multiple records/logs;
- free-text secret scanning is not an authorization boundary.

**5. Minimum durable fields**

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

**6. Storage / API behavior**

- append-only owner-local storage;
- Accepted/duplicate behavior is deterministic across process restart;
- no coding-agent execution is started by this ingress;
- no remote/backend call is performed by this ingress;
- ordinary observation/context readers may see safe structural metadata but not the raw body;
- storage failure does not mutate Task/Execution state.

The exact internal Rust function/tool shape may follow the existing observation store; do not add a public raw-prompt retrieval tool in this packet.

## 受け入れ条件

Complete source criteria from “7. Acceptance” (unchecked items remain unverified):

- [x] first append creates one durable observation and owner-only content reference.
- [x] same `agent + conversation + source_event` replay creates no duplicate.
- [x] same source identity with conflicting immutable envelope fails closed.
- [x] restart + replay preserves idempotency.
- [x] unbound prompt append is accepted without fabricating task/execution identity.
- [x] raw prompt body is absent from ordinary MCP/HTTP/Fabric-safe projections and logs.
- [x] system/developer/hidden-reasoning event kinds are not accepted through the user-prompt contract.
- [x] storage failure does not change task/backend execution state.
- [x] legacy observation records remain readable.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 8. Tests

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

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 検証記録

- 2026-10-06: durable append/replay/conflict/restart, owner-only raw references, unbound events and rejected hidden event kinds PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/prompt_ingress.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.

## 既存設計・履歴

> Historical Status: ready
Repository: `f4ah6o/temote-mcp`
> Historical Created: 2026-10-01 (Asia/Tokyo)
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
