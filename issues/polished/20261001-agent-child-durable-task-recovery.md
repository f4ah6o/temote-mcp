# LC3: preserve durable task recovery after provider runtime loss

Status: polished
Model: unknown
Created: 2026-10-01
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Keep retained tasks discoverable and recoverable after a provider child runtime exits.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Keep retained tasks discoverable and recoverable after a provider child runtime exits.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 7. Non-goals

- General task waiting/polling optimization.
- New cross-backend task migration.
- Attributing the historical Devin incident to the verified equivalent source defect without incident evidence.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Preserved fixed contract: 3. Fixed design

- Durable task record remains the retained source for task identity/status after runtime loss.
- `task_list` must continue to rediscover the retained owned task.
- A read/recovery path must distinguish:
  - runtime can be safely reconstructed/reconciled,
  - task is terminal after reconciliation,
  - runtime state is uncertain and requires reconciliation,
  - original owner is closing and recovery is temporarily unavailable.
- Do not fabricate a completed/failed state merely because the process is gone.
- Do not attach an old task/runtime to a same-id replacement session; validate full `SessionInstance`, canonical scope, and generation before recovery.
- Recovery must not duplicate the original start/control side effect.

## 受け入れ条件

Complete source criteria from “5. Acceptance” (unchecked items remain unverified):

- [ ] A retained non-terminal task remains visible through the supported list/lookup surface after provider runtime loss.
- [ ] Safe runtime reconstruction/reconciliation can recover the original task without creating a second task or turn.
- [ ] An uncertain state is reported explicitly instead of being guessed terminal.
- [ ] Closing-owner state is distinguishable from missing-task state.
- [ ] Same-id replacement sessions cannot access or adopt the prior instance's task/runtime.
- [ ] Operation receipt/replay protection remains intact across recovery.
- [ ] Terminal scoped evidence behavior remains unchanged.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd gateway && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 6. Tests

Add focused crash/runtime-loss fixtures for Codex, OpenCode, and Devin ACP, including same-id replacement isolation, then run:

- [ ] focused retained-task recovery tests PASS
- [ ] `cargo fmt --all -- --check` PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.

## 既存設計・履歴

> Historical Status: ready
Repository: `f4ah6o/temote-mcp`
> Historical Created: 2026-10-01 (Asia/Tokyo)
Parent/source: `issues/closed/20260927-agent-child-lifecycle-cleanup.md`
Related: `issues/polished/20261001-agent-child-cleanup-retry-and-provider-aggregation.md`

## 1. Goal

A non-terminal Codex/OpenCode/Devin ACP task record must remain discoverable and recoverable through the supported task lifecycle when its provider child runtime exits or cleanup is retried. Runtime loss must not turn durable task metadata into an inaccessible orphan.

## 2. Confirmed risk

The source investigation found that non-terminal durable records can outlive the child runtime while the same owner remains in a closing lifecycle state. Normal task APIs can then become unusable even though the task record still exists.

## 3. Fixed design

- Durable task record remains the retained source for task identity/status after runtime loss.
- `task_list` must continue to rediscover the retained owned task.
- A read/recovery path must distinguish:
  - runtime can be safely reconstructed/reconciled,
  - task is terminal after reconciliation,
  - runtime state is uncertain and requires reconciliation,
  - original owner is closing and recovery is temporarily unavailable.
- Do not fabricate a completed/failed state merely because the process is gone.
- Do not attach an old task/runtime to a same-id replacement session; validate full `SessionInstance`, canonical scope, and generation before recovery.
- Recovery must not duplicate the original start/control side effect.

## 4. Scope

Implement the smallest common behavior needed by Codex/OpenCode/Devin ACP retained task APIs. Avoid adding a new generic execution API in this packet.

## 5. Acceptance

- [ ] A retained non-terminal task remains visible through the supported list/lookup surface after provider runtime loss.
- [ ] Safe runtime reconstruction/reconciliation can recover the original task without creating a second task or turn.
- [ ] An uncertain state is reported explicitly instead of being guessed terminal.
- [ ] Closing-owner state is distinguishable from missing-task state.
- [ ] Same-id replacement sessions cannot access or adopt the prior instance's task/runtime.
- [ ] Operation receipt/replay protection remains intact across recovery.
- [ ] Terminal scoped evidence behavior remains unchanged.

## 6. Tests

Add focused crash/runtime-loss fixtures for Codex, OpenCode, and Devin ACP, including same-id replacement isolation, then run:

- [ ] focused retained-task recovery tests PASS
- [ ] `cargo fmt --all -- --check` PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## 7. Non-goals

- General task waiting/polling optimization.
- New cross-backend task migration.
- Attributing the historical Devin incident to the verified equivalent source defect without incident evidence.
