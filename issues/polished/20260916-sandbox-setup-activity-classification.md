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
