# CC1: explicit Codex conversation continuation for a new task

Status: ready
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `c305e41`
Parent issue: `issues/open/20260925-agent-conversation-continuation.md`
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
   (`ensure_task_owner`-equivalent rules); read its retained `thread_id`.
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
