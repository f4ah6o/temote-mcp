# LC1: make agent-child watcher liveness tri-state and fail safe on probe errors

Status: done
Model: unknown
Created: 2026-10-01
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Treat liveness probe errors as unknown so watchers shut down children only on confirmed inactivity or replacement.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Treat liveness probe errors as unknown so watchers shut down children only on confirmed inactivity or replacement.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 7. Non-goals

- Retryable cleanup after a cleanup operation has already started (LC2).
- Durable task recovery after the runtime is already gone (LC3).
- Claiming that the historical Devin incident was caused by this source defect without incident evidence.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Preserved fixed contract: 3. Fixed design

Represent watcher observation explicitly:

```text
active
inactive
replaced
unknown(error)
```

- `active`: retain the current child.
- `inactive`: watcher-driven cleanup may start after the existing bounded/debounce policy.
- `replaced`: old-generation cleanup may start; preserve full `SessionInstance` fencing.
- `unknown`: do not begin shutdown. Retry later and retain explicit supervisor shutdown as the authoritative escape path.
- Do not weaken canonical scope, generation, or runtime-lease checks.

Apply the same semantics to Codex, OpenCode, and Devin ACP watcher paths rather than fixing only one adapter.

## 受け入れ条件

Complete source criteria from “5. Acceptance” (unchecked items remain unverified):

- [x] A transient metadata-read error does not stop an active Codex child.
- [x] A transient metadata-read error does not stop an active OpenCode child.
- [x] A transient metadata-read error does not stop an active Devin ACP child.
- [x] A transient liveness-probe error is reported/treated as unknown, not inactive.
- [x] Confirmed parent inactivity still triggers watcher cleanup.
- [x] Confirmed same-id generation replacement still fences and cleans the old child.
- [x] Explicit supervisor shutdown remains authoritative.
- [x] Existing `SessionInstance`, canonical-scope, and runtime-lease isolation tests remain green.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 6. Tests

Add deterministic watcher tests for active / inactive / replaced / unknown for all three provider adapters, then run:

- [ ] focused Codex watcher tests PASS
- [ ] focused OpenCode watcher tests PASS
- [ ] focused Devin ACP watcher tests PASS
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

- 2026-10-06: unknown observations retain owner; confirmed inactivity/replacement cleanup and authoritative shutdown fixtures PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/codex_app_server.rs; src/opencode_server.rs; src/devin_acp.rs
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

## 1. Goal

Codex, OpenCode, and Devin ACP child-runtime watchers must initiate provider-child shutdown only from confirmed parent-session inactivity or confirmed generation replacement. Metadata/liveness probe errors are `unknown`, never evidence that the parent is inactive.

## 2. Confirmed defect

The current watcher path uses a boolean active check and collapses errors with behavior equivalent to `session_is_active(...).await.unwrap_or(false)`. A transient metadata or liveness error can therefore be interpreted as inactivity and begin child shutdown while the parent session is still active.

## 3. Fixed design

Represent watcher observation explicitly:

```text
active
inactive
replaced
unknown(error)
```

- `active`: retain the current child.
- `inactive`: watcher-driven cleanup may start after the existing bounded/debounce policy.
- `replaced`: old-generation cleanup may start; preserve full `SessionInstance` fencing.
- `unknown`: do not begin shutdown. Retry later and retain explicit supervisor shutdown as the authoritative escape path.
- Do not weaken canonical scope, generation, or runtime-lease checks.

Apply the same semantics to Codex, OpenCode, and Devin ACP watcher paths rather than fixing only one adapter.

## 4. Scope

Expected code surface:

- `src/codex_app_server.rs`
- `src/opencode_server.rs`
- `src/devin_acp.rs`
- shared session/liveness helper only if necessary for one common tri-state result

Devin ACP has the same metadata/error-collapse watcher path in `wait_for_session_stop`; include that source defect. Attribution of the historical Devin incident remains unverified.

## 5. Acceptance

- [ ] A transient metadata-read error does not stop an active Codex child.
- [ ] A transient metadata-read error does not stop an active OpenCode child.
- [ ] A transient metadata-read error does not stop an active Devin ACP child.
- [ ] A transient liveness-probe error is reported/treated as unknown, not inactive.
- [ ] Confirmed parent inactivity still triggers watcher cleanup.
- [ ] Confirmed same-id generation replacement still fences and cleans the old child.
- [ ] Explicit supervisor shutdown remains authoritative.
- [ ] Existing `SessionInstance`, canonical-scope, and runtime-lease isolation tests remain green.

## 6. Tests

Add deterministic watcher tests for active / inactive / replaced / unknown for all three provider adapters, then run:

- [ ] focused Codex watcher tests PASS
- [ ] focused OpenCode watcher tests PASS
- [ ] focused Devin ACP watcher tests PASS
- [ ] `cargo fmt --all -- --check` PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## 7. Non-goals

- Retryable cleanup after a cleanup operation has already started (LC2).
- Durable task recovery after the runtime is already gone (LC3).
- Claiming that the historical Devin incident was caused by this source defect without incident evidence.
