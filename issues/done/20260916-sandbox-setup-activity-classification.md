# Separate sandbox setup failure from child process failure in activity

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Source issue: `issues/closed/20260916-sandbox-setup-failure-misclassified-as-child-failed.md`
Depends on: Phase 0 OpenCode canary

## Goal

Use typed internal evidence so failures before child start are not recorded as `ActivityErrorKind::ChildFailed`.

## Scope

- trace `spawn_sandboxed_command_with_controls` -> `run_session_command` -> activity finalization;
- add the smallest typed outcome needed to distinguish setup/spawn failure from an exited child;
- preserve cancellation and timeout semantics;
- update gateway/contract snapshot only if the public enum/shape changes.

Do not classify by matching stderr strings. Do not redesign activity architecture.

## Acceptance

- pre-child sandbox/runtime setup failure maps to a fixed non-`ChildFailed` activity kind;
- non-zero child exit remains `ChildFailed`;
- cancellation/timeout tests remain green;
- focused activity/job tests pass;
- `just sandboxed-check` passes.

## Implementation notes (2026-09-16)

Current `main` did not already cover this: `spawn_sandboxed_command_with_controls` collapsed `run_session_command` errors (canonicalize, writable-root validation, policy/cache setup, spawn) and `render_output` non-zero-exit errors into the same `JobActivityOutcome::Failed`, so `finish_job_activity` always recorded `ActivityErrorKind::ChildFailed`.

Changes:

- `src/activity/contract.rs`: added the fixed `ActivityErrorKind::SandboxSetupFailed` variant with wire name `sandbox_setup_failed`, safe summary `error=sandbox_setup_failed`, deserialization mapping, and fixed-summary/JSON round-trip assertions.
- `src/mcp.rs`: added `JobActivityFailure { ChildFailed, SandboxSetupFailed }`; the sandboxed-command worker now classifies `Ok(output)` + `render_output` as `ChildFailed` and `Err` from `run_session_command` as `SandboxSetupFailed`; `finish_job_activity` maps the typed failure to the activity error kind. Cancellation/timeout select arms are unchanged.
- The `local_agent` and `dev_tool` workers keep `ChildFailed` for all failures; only the packet's `spawn_sandboxed_command_with_controls` path changed.
- No gateway/contract snapshot update: `ActivityErrorKind` is not part of the gateway routed-tool contract, and the activity contract change is additive.

Observed behavior:

- New focused test `activity_job_sandbox_setup_failure_is_not_child_failed` uses an existing cwd plus a missing permitted root (a real pre-child `validate_writable_scope` failure) and asserts the terminal activity summary is `error=sandbox_setup_failed`; the child never spawns.
- Existing `activity_job_foreground_completion_and_child_failure_are_terminalized` still asserts `exit 7` -> `error=child_failed`.
- Session-stop and lifetime tests still assert `reason=session_stopped` / `reason=timeout`.
- No stderr string matching was introduced.

Gates:

- `cargo test --lib activity --locked`: PASS (61/61).
- `cargo test --bin temote-mcp activity_job --locked`: PASS (6/6, including the new test).
- `just sandboxed-check`: exit 0 (first run surfaced only a `cargo fmt` difference in the new test, fixed with `cargo fmt --all`; subsequent fmt-check PASS).
- host/CI-only: NOT RUN (Linux nested sandbox runtime tests, full binary/local Unix-socket integration suite, ignored supervisor/process-boundary E2E).

## 2026-09-16 completion

All packet acceptance items are met repository-locally. The source issue `issues/closed/20260916-sandbox-setup-failure-misclassified-as-child-failed.md` is already superseded by this packet and stays closed.
