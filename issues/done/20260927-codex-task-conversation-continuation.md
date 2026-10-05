# CC1: explicit Codex conversation continuation for a new task

Status: done
Model: unknown
Created: 2026-09-27
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Continue a Codex conversation in a new Temote task with independent task identity and atomic successor ownership.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Continue a Codex conversation in a new Temote task with independent task identity and atomic successor ownership.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Preserved fixed contract: 2. Fixed decisions

- New typed option on `codex_task_start`: e.g. `continuation` with
  `{"type": "new"}` (default, unchanged) or `{"type": "previous_task",
  "task_id": <uuid>}`. No free-form backend command and no caller-supplied
  Codex `thread_id` (parent §3.2).
- Resolution rules (parent §4): same authorized session/scope only; backend
  must be Codex; take the retained `thread_id` from task A's record
  (`TaskRecord.thread_id`, `src/codex_app_server.rs:366`) — never from caller
  input; `thread/resume` then a fresh `turn/start` for task B.
- Fail closed: non-resumable/absent conversation is an explicit error or an
  `unsupported` capability result — never a silent fresh thread.
- Conversation fencing + explicit handoff: continuation is only legal when
  the source task is quiescent — `record.status.is_terminal()` AND no
  in-flight turn/control operation on A's record (terminal status implies no
  in-flight turn). Runtime leases are keyed by `task_id`
  (`runtime-locks/<task_id>.lock`, `src/codex_app_server.rs:516`), so task B
  gets a different lock file from A.
  - Terminal does NOT imply lease-free: `insert_runtime_unchecked`
    (`src/codex_app_server.rs:1819`) keeps the `RuntimeHandle` (holding its
    `_lease`) in `runtimes()` until `CHILD_LIFETIME` (line 42, 2h) or session
    stop; reconciliation marking A `completed` does not remove it.
  - Atomic conversation claim: A's record gains a `continued_by_task_id` (or
    handoff-generation) field claimed under `store_lock` in the same locked
    load-validate-save mutation as the owner/scope/quiescence check —
    continuing sets it to B's `task_id` only when unset. A second concurrent
    successor (C) sees the claim held and fails closed; a replay of B's own
    `operation_id`/`task_id` against the held claim is idempotent (no second
    turn). This is the store-level serialization point for successor claims —
    retiring A's runtime alone does not stop two successors resolving a
    terminal A simultaneously.
  - Two-phase retirement: while holding the claim under `store_lock`, detach
    A's `RuntimeHandle` from `runtimes()` and *retain* its
    `Arc<TaskRuntimeLease>`; release the lock; `await client.shutdown()`; only
    then drop the lease (fd close → flock release). Releasing the lease before
    shutdown would permit a cross-process reacquisition mid-handoff.
  - After the handoff, B spawns its own runtime, `thread/resume`s A's retained
    `thread_id`, and opens a new `turn/start`. If A is non-terminal, already
    claimed by another task, or its runtime cannot be retired cleanly, B's
    start fails closed with an explicit error — two task runtimes can never
    drive the same Codex thread concurrently.
  - (rejected alternatives: refusing while the terminal runtime is still
    registered — blocks the primary implement→follow-up flow for up to 2h; or
    a thread/conversation-level lease serializing turn ownership across tasks
    — more machinery, revisit only if concurrent multi-task threads become a
    goal.)
- Task B gets its own `task_id`, record, receipts, and bounded lineage metadata
  (`continued_from_task_id` on the record; see parent §6). No inheritance of
  A's status / verification / delivery state.
- Idempotent `operation_id` semantics are preserved for the continued start.

## 受け入れ条件

Complete source criteria from “5. Acceptance” (unchecked items remain unverified):

- [x] Default start (no `continuation`) still calls `thread/start`.
- [x] Continued start issues no `thread/start`; it resumes A's thread and opens
      a new turn for B.
- [x] A completing/failing does not move B's status; B inherits no
      verification or delivery state.
- [x] Idempotent replay of a continued start does not duplicate the turn.
- [x] Cross-session / wrong-backend / non-resumable continuations fail closed
      with explicit errors.
- [x] Continuation while the source task is non-terminal or has an in-flight
      turn/control fails closed — two task runtimes can never drive the same
      Codex thread concurrently.
- [x] A just became completed while its runtime is still registered → B can
      safely continue immediately: the handoff retires A's runtime/lease and
      B's fresh runtime resumes the thread.
- [x] Race: two concurrent continuations B and C of the same terminal A —
      exactly one acquires the conversation (persisted `continued_by_task_id`
      == winner's task_id); the loser fails closed with an explicit error and
      no `thread/resume` is issued twice.
- [x] Same-task runtime reconstruction via `thread/resume` is unchanged.
- [x] Gateway contract/fingerprint regenerated; tool count updated.
- [x] Old task records without lineage fields still load.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 6. Validation commands

- `cargo test --bin temote --all-features --locked codex`
- `cargo test --bin temote --all-features --locked orchestration`
- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT=1 cargo test --bin temote --all-features --locked gateway`
- `(cd fabric && npm test)`
- `just sandboxed-check`
- Live: continuation against a real `codex app-server` — live-matrix row;
  NOT RUN here.

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 検証記録

- 2026-10-06: Continuation fixtures and final local↔MCP live acceptance PASS: same-thread new-turn successor, independent task state, exact replay and fenced source controls.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.
- 2026-10-06: Continuation fixtures and final local↔MCP live acceptance PASS: same-thread new-turn successor, independent task state, exact replay and fenced source controls.
- 2026-10-06: Completed after actual two-frontend native Codex acceptance and central regression gates.

## 既存設計・履歴

> Historical Status: ready (revised 2026-09-27 per PR #73 review: conversation fencing +
explicit terminal-runtime handoff + atomic successor claim — the handoff
alone does not serialize two concurrent continuations of the same task).
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `c305e41`
Parent issue: `issues/closed/20260925-agent-conversation-continuation.md`
Prerequisites: orchestration A1/A2 slices on `main`
(`src/orchestration.rs`, `src/orchestration/requests.rs`)

## 1. Goal

A caller can start a new Temote task that continues a *previous task's* Codex
conversation (`continue_from = <task_id>`), while keeping Temote task identity,
verification, delivery, receipts, and ownership fully independent.

## 2. Fixed decisions

- New typed option on `codex_task_start`: e.g. `continuation` with
  `{"type": "new"}` (default, unchanged) or `{"type": "previous_task",
  "task_id": <uuid>}`. No free-form backend command and no caller-supplied
  Codex `thread_id` (parent §3.2).
- Resolution rules (parent §4): same authorized session/scope only; backend
  must be Codex; take the retained `thread_id` from task A's record
  (`TaskRecord.thread_id`, `src/codex_app_server.rs:366`) — never from caller
  input; `thread/resume` then a fresh `turn/start` for task B.
- Fail closed: non-resumable/absent conversation is an explicit error or an
  `unsupported` capability result — never a silent fresh thread.
- Conversation fencing + explicit handoff: continuation is only legal when
  the source task is quiescent — `record.status.is_terminal()` AND no
  in-flight turn/control operation on A's record (terminal status implies no
  in-flight turn). Runtime leases are keyed by `task_id`
  (`runtime-locks/<task_id>.lock`, `src/codex_app_server.rs:516`), so task B
  gets a different lock file from A.
  - Terminal does NOT imply lease-free: `insert_runtime_unchecked`
    (`src/codex_app_server.rs:1819`) keeps the `RuntimeHandle` (holding its
    `_lease`) in `runtimes()` until `CHILD_LIFETIME` (line 42, 2h) or session
    stop; reconciliation marking A `completed` does not remove it.
  - Atomic conversation claim: A's record gains a `continued_by_task_id` (or
    handoff-generation) field claimed under `store_lock` in the same locked
    load-validate-save mutation as the owner/scope/quiescence check —
    continuing sets it to B's `task_id` only when unset. A second concurrent
    successor (C) sees the claim held and fails closed; a replay of B's own
    `operation_id`/`task_id` against the held claim is idempotent (no second
    turn). This is the store-level serialization point for successor claims —
    retiring A's runtime alone does not stop two successors resolving a
    terminal A simultaneously.
  - Two-phase retirement: while holding the claim under `store_lock`, detach
    A's `RuntimeHandle` from `runtimes()` and *retain* its
    `Arc<TaskRuntimeLease>`; release the lock; `await client.shutdown()`; only
    then drop the lease (fd close → flock release). Releasing the lease before
    shutdown would permit a cross-process reacquisition mid-handoff.
  - After the handoff, B spawns its own runtime, `thread/resume`s A's retained
    `thread_id`, and opens a new `turn/start`. If A is non-terminal, already
    claimed by another task, or its runtime cannot be retired cleanly, B's
    start fails closed with an explicit error — two task runtimes can never
    drive the same Codex thread concurrently.
  - (rejected alternatives: refusing while the terminal runtime is still
    registered — blocks the primary implement→follow-up flow for up to 2h; or
    a thread/conversation-level lease serializing turn ownership across tasks
    — more machinery, revisit only if concurrent multi-task threads become a
    goal.)
- Task B gets its own `task_id`, record, receipts, and bounded lineage metadata
  (`continued_from_task_id` on the record; see parent §6). No inheritance of
  A's status / verification / delivery state.
- Idempotent `operation_id` semantics are preserved for the continued start.

## 3. Read / change scope

- `src/codex_app_server.rs`: `TaskRecord` (line ~356, `thread_id` at 366),
  store ops (`load` 583, `save` 619, `update` 658), `ensure_task_owner`
  (line 1279), `thread/start` + `thread/resume` call sites, task-start entry.
- `src/orchestration/requests.rs`: `Backend`, `Operation::TaskStart` parse
  (line ~321), `BackendCapabilities` (line 253) — add a continuation capability
  bit so non-Codex backends report `unsupported` rather than ignoring the flag.
- `src/mcp.rs`: `codex_task_start` input schema (line ~781) gains the optional
  typed `continuation` object — gateway contract regen required.
- `gateway/src/protocol.js`: `PUBLIC_TOOLS` mirror; contract snapshot +
  fingerprint regen; bump test count in `gateway/test/protocol.test.mjs`.

## 4. Steps

1. Extend the tool schema and orchestration request type for `continuation`.
2. Resolve `previous_task` through the store with owner/scope checks
   (`ensure_task_owner`-equivalent rules); under `store_lock`, enforce the
   fencing gate (terminal, no in-flight turn/control) and atomically claim the
   conversation by setting `continued_by_task_id` on A's record
   (`#[serde(default)]` field), then read its retained `thread_id`.
   Two-phase-retire A's still-registered runtime: detach the handle under the
   lock while retaining the lease, release the lock, await shutdown, drop the
   lease last.
3. Drive `thread/resume` + new `turn/start`; persist B's lineage metadata with
   `#[serde(default)]` for old records.
4. Reject cross-session, backend-mismatch, and unresumable cases explicitly.
5. Focused tests; `just sandboxed-check`; host-only/live rows recorded as NOT
   RUN and added to the live matrix.

## 5. Acceptance

- [ ] Default start (no `continuation`) still calls `thread/start`.
- [ ] Continued start issues no `thread/start`; it resumes A's thread and opens
      a new turn for B.
- [ ] A completing/failing does not move B's status; B inherits no
      verification or delivery state.
- [ ] Idempotent replay of a continued start does not duplicate the turn.
- [ ] Cross-session / wrong-backend / non-resumable continuations fail closed
      with explicit errors.
- [ ] Continuation while the source task is non-terminal or has an in-flight
      turn/control fails closed — two task runtimes can never drive the same
      Codex thread concurrently.
- [ ] A just became completed while its runtime is still registered → B can
      safely continue immediately: the handoff retires A's runtime/lease and
      B's fresh runtime resumes the thread.
- [ ] Race: two concurrent continuations B and C of the same terminal A —
      exactly one acquires the conversation (persisted `continued_by_task_id`
      == winner's task_id); the loser fails closed with an explicit error and
      no `thread/resume` is issued twice.
- [ ] Same-task runtime reconstruction via `thread/resume` is unchanged.
- [ ] Gateway contract/fingerprint regenerated; tool count updated.
- [ ] Old task records without lineage fields still load.

## 6. Validation commands

- `cargo test --bin temote-mcp --all-features --locked codex`
- `cargo test --bin temote-mcp --all-features --locked orchestration`
- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT=1 cargo test --bin temote-mcp --all-features --locked gateway`
- `(cd gateway && npm test)`
- `just sandboxed-check`
- Live: continuation against a real `codex app-server` — live-matrix row;
  NOT RUN here.

## 7. Delivery authorization

One feature branch + one PR to `main`, including the regenerated gateway
contract artifacts.

## 8. Completion report

(to be filled by the implementing packet run)

### Final live acceptance (2026-10-06)

The isolated Codex 0.160.0 canary passed local→MCP and MCP→local control. Source task `c4e2d3fa-bb65-51a0-9dff-f345b501eff9` and successor `24c0632b-2354-5888-90ff-6f7fecac5797` share one thread and independent turns/receipts. Native reports were valid; verification remained not_run and delivery not_started. A 3,464-byte evidence record was read through the owning supervisor. Full Rust tests (125 library, 1,047 binary and ordinary integrations), Clippy, fmt, no-default check and diff gate passed. See the integration evaluation for the initial failure and repair.
