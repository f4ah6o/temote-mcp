# session-GC restore-plan read races parallel tests in the shared private test state root

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/done/20260905-04-session-metadata-retention.md` (test-isolation follow-up)
Depends on: none

## Observation (2026-09-17, full parallel binary suite)

`cargo test --all-targets --all-features --locked` failed once in three runs with:

```text
session_control::tests::host_liveness_tests::session_gc_ordering_and_limit_are_deterministic
panicked at src/session_control.rs:6610:74:
called `Result::unwrap()` on an `Err` value: a supervisor restore plan could not be read safely
Caused by: invalid supervisor restore plan: missing field `plan_schema` at line 1 column 2
```

The same full-suite invocation passed with only the two known app-server failures on the other two
runs, and the test always passes when selected alone.

## Current code and contract

`session_gc_*` tests call `run_session_gc` -> `build_session_gc_plan` ->
`protected_upgrade_session_ids` (`src/session_control.rs:3930`), which enumerates and parses
`restore-*.json` files in `upgrade_plan_directory()`. Under `cfg(test)`, `config::state_dir()` is
the process-wide `test_support::private_process_root()`, so unrelated tests in the same binary
(upgrade/supervisor tests) can create or rewrite a restore-plan file concurrently. A read that
observes a partially-written file fails the whole session-GC plan instead of tolerating a
concurrent writer.

This is a test-isolation defect (parallel tests sharing one state root), not a sandbox/socket
issue; it is distinct from `issues/polished/20260917-session-list-supervisor-owned-missing-cwd.md`.

## The one responsibility to change

Make the session-GC plan read of supervisor restore plans tolerant of concurrent test writers, or
serialize restore-plan writers/readers in tests (for example an explicit shared lock or a
per-test state root), without weakening production restore-plan validation:

- production `protected_upgrade_session_ids` keeps failing closed on malformed plans outside the
  test process-shared root;
- the focused test must still detect a genuinely invalid restore plan.

## Not changing

- Production restore-plan schema/validation and upgrade behavior.
- Session/worktree state: no deletion or mutation.

## Verification

- repeated full parallel binary-suite runs (for example 10) without this failure;
- focused test that a deliberately malformed restore plan still fails closed;
- `just sandboxed-check` and CI full suite unchanged elsewhere.

## Completion condition

The full parallel suite is stable for this test while malformed restore plans still fail closed.
