# BW1: keep delegated-task revision stable across unchanged reconciliation

Status: ready
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
Parent: `issues/open/20260927-bounded-wait-for-delegated-tasks.md`

## 1. Goal

Make `after_revision` / `not_modified` reliable by preventing `*_task_get` from advancing the task's public revision when reconciliation observes no meaningful change.

This is a prerequisite for bounded waiting; it is useful independently because callers can already send `after_revision`.

## 2. Observed problem

Dogfood recorded repeated `running` reads whose revisions advanced even though the visible task state did not meaningfully change. Revision churn defeats compact `not_modified` responses and makes a later long-poll cursor ambiguous.

## 3. Semantic revision contract

A task revision advances only when the persisted/public task meaning changes.

Examples that **do** advance revision:

- task status/state transition;
- terminal report / report-status / raw-result metadata change;
- last error or reconciliation state change;
- backend conversation/session binding change;
- usage/model/effort value changes that are part of the public task view;
- pending-interaction summary semantic change;
- terminal/scoped evidence reference change;
- control/start receipt state when it changes the caller-visible task record.

Examples that **do not** advance revision:

- a reconciliation attempt reads the same backend state;
- heartbeat/observation timestamp refresh that is not part of task semantic state;
- rewriting an identical normalized value;
- `task_get` with no backend-visible change.

Do not suppress real state changes merely to reduce polling.

## 4. Backend parity

Apply the same semantic-revision rule to:

- Codex app-server
- OpenCode serve
- Devin ACP
- Devin Cloud

Backend-specific records may retain private timestamps/diagnostics, but the public task revision used by `after_revision` must follow the common semantic contract.

## 5. Acceptance

- [ ] two consecutive unchanged `task_get` calls preserve the same revision for each backend.
- [ ] `task_get(after_revision=current)` returns compact `not_modified` when reconciliation is unchanged.
- [ ] running→waiting/terminal/error transitions increment revision.
- [ ] report/evidence/usage/pending-interaction semantic changes increment revision when exposed to callers.
- [ ] a transport/backend read failure that changes reconciliation state is not hidden as `not_modified`.
- [ ] task ownership, session generation, and scope validation are unchanged.
- [ ] task reads never start/duplicate a task because of this change.

## 6. Tests

Use fake backend/runtime fixtures to perform unchanged reconciliation twice and then one real semantic transition for each backend.

- [ ] Codex focused tests PASS
- [ ] OpenCode focused tests PASS
- [ ] Devin ACP focused tests PASS
- [ ] Devin Cloud focused tests PASS
- [ ] `cargo fmt --all -- --check` PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] gateway contract tests PASS if schema/metadata changes
- [ ] `git diff --check` PASS

## 7. Non-goals

- Adding `wait_ms` or a new `task_wait` operation in this packet.
- Changing task start/control idempotency.
- Reducing backend polling frequency by itself.

The parent chooses the bounded-wait transport/API only after BW1 makes the cursor meaningful.
