# LC2: make agent-child cleanup retryable and attempt every provider

Status: done
Model: unknown
Created: 2026-10-01
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Retry same-owner child cleanup safely and attempt all providers before reporting aggregated failures.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Retry same-owner child cleanup safely and attempt all providers before reporting aggregated failures.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 7. Non-goals

- Treating probe errors as inactivity (LC1).
- Defining task recovery semantics once a child runtime is gone (LC3).
- Weakening the closing fence to make retries easier.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Preserved fixed contract: 3. Fixed design

- Keep the closing fence after partial cleanup failure; do not reopen the old owner to new work.
- Permit a later cleanup call for the **same full SessionInstance** to resume the idempotent cleanup steps.
- Cleanup for a different generation remains rejected/fenced.
- Mark cleanup complete only after provider drain/finalization succeeds.
- In supervisor removal, attempt Codex, OpenCode, and Devin ACP cleanup independently and aggregate/return the resulting errors after all three attempts.
- Do not convert uncertain cleanup into success.
- Preserve runtime-lease ownership and canonical-scope checks on every retry.

## 受け入れ条件

Complete source criteria from “5. Acceptance” (unchecked items remain unverified):

- [x] A drain timeout leaves the original owner fenced and a later same-owner cleanup retry can finish.
- [x] A task-store-open/finalization error leaves the original owner fenced and retryable.
- [x] A replacement `SessionInstance` cannot take over the old cleanup attempt or lease.
- [x] Successful retry clears the closing lifecycle state exactly once.
- [x] Supervisor removal attempts OpenCode and Devin ACP cleanup even when Codex fails, and attempts Devin ACP even when OpenCode fails.
- [x] Multiple provider errors are retained/returned without silently dropping the later provider's result.
- [x] Repeated cleanup is idempotent after completion.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 6. Tests

- [ ] drain failure → retry → success
- [ ] task-store/finalization failure → retry → success
- [ ] same-id replacement cannot retry old-owner cleanup
- [ ] Codex cleanup failure still attempts OpenCode and Devin ACP cleanup
- [ ] OpenCode cleanup failure still attempts Devin ACP cleanup
- [ ] `cargo fmt --all -- --check` PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 検証記録

- 2026-10-06: same-owner retry, closing fences, replacement isolation and independent provider cleanup/error aggregation PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/codex_app_server.rs; src/opencode_server.rs; src/devin_acp.rs; src/supervisor.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.

## 既存設計・履歴

> Historical Status: ready
Repository: `f4ah6o/temote-mcp`
> Historical Created: 2026-10-01 (Asia/Tokyo)
Parent/source: `issues/closed/20260927-agent-child-lifecycle-cleanup.md`
Depends on: LC1 is independent; either packet may land first if tests preserve the same lifecycle contract.

## 1. Goal

Once provider-child shutdown begins, cleanup must be safely retryable for the same full `SessionInstance`, and supervisor removal must attempt every provider even if one provider cleanup fails.

## 2. Confirmed defects

- A lifecycle entry can remain `closing = true` indefinitely when drain, task-store open, or finalization fails before `finish_session_shutdown`.
- The watcher logs that failure but does not establish a retry path.
- Calls for the same owner then continue failing as "session instance is closing".
- `supervisor::remove_agent_sessions` can return after Codex cleanup fails and skip OpenCode and Devin ACP cleanup; an OpenCode failure can also skip Devin ACP cleanup.

## 3. Fixed design

- Keep the closing fence after partial cleanup failure; do not reopen the old owner to new work.
- Permit a later cleanup call for the **same full SessionInstance** to resume the idempotent cleanup steps.
- Cleanup for a different generation remains rejected/fenced.
- Mark cleanup complete only after provider drain/finalization succeeds.
- In supervisor removal, attempt Codex, OpenCode, and Devin ACP cleanup independently and aggregate/return the resulting errors after all three attempts.
- Do not convert uncertain cleanup into success.
- Preserve runtime-lease ownership and canonical-scope checks on every retry.

## 4. Scope

Expected code surface:

- provider lifecycle/cleanup code in `src/codex_app_server.rs`
- provider lifecycle/cleanup code in `src/opencode_server.rs`
- provider lifecycle/cleanup code in `src/devin_acp.rs`
- `src/supervisor.rs::remove_agent_sessions`
- focused lifecycle tests

## 5. Acceptance

- [ ] A drain timeout leaves the original owner fenced and a later same-owner cleanup retry can finish.
- [ ] A task-store-open/finalization error leaves the original owner fenced and retryable.
- [ ] A replacement `SessionInstance` cannot take over the old cleanup attempt or lease.
- [ ] Successful retry clears the closing lifecycle state exactly once.
- [ ] Supervisor removal attempts OpenCode and Devin ACP cleanup even when Codex fails, and attempts Devin ACP even when OpenCode fails.
- [ ] Multiple provider errors are retained/returned without silently dropping the later provider's result.
- [ ] Repeated cleanup is idempotent after completion.

## 6. Tests

- [ ] drain failure → retry → success
- [ ] task-store/finalization failure → retry → success
- [ ] same-id replacement cannot retry old-owner cleanup
- [ ] Codex cleanup failure still attempts OpenCode and Devin ACP cleanup
- [ ] OpenCode cleanup failure still attempts Devin ACP cleanup
- [ ] `cargo fmt --all -- --check` PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## 7. Non-goals

- Treating probe errors as inactivity (LC1).
- Defining task recovery semantics once a child runtime is gone (LC3).
- Weakening the closing fence to make retries easier.
