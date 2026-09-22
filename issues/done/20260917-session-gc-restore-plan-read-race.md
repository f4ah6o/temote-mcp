# session-GC restore-plan read races parallel tests in the shared private test state root

Status: done
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
issue; it is distinct from `issues/done/20260917-session-list-supervisor-owned-missing-cwd.md`.

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

## Implementation — 2026-09-22 (Devin session)

Serialization chosen over read-tolerance, so production `read_upgrade_plan` and the fail-closed
scan semantics are byte-identical outside the test binary:

- `upgrade_plan_test_lock` (`src/session_control.rs`, `#[cfg(test)]` static `Mutex`) serializes
  restore-plan writers and the session-GC scan inside the shared per-process test root:
  - `write_upgrade_plan` and `remove_upgrade_plan` take it for their whole operation;
  - `protected_upgrade_session_ids` takes it across the full enumeration + reads, via a new
    thin wrapper over `scan_upgrade_restore_plans` (the former body — a scan can no longer
    interleave with a writer between `read_dir` and each file read);
  - `upgrade_failure_report_rejects_outside_paths_symlinks_and_oversize` holds it for its whole
    body since it parks an intentionally invalid `restore-*.json` fixture in the shared root.
- `read_upgrade_plan` itself takes no lock: it only ever reads caller-chosen, uuid-unique paths
  that are already fully written, so per-file locking could not close the enumerate-vs-remove
  window anyway; locking the scan does.
- New focused test `restore_plan_scan_still_fails_closed_on_a_malformed_plan` parks a malformed
  plan under the guard and asserts `scan_upgrade_restore_plans` still errors with "a supervisor
  restore plan could not be read safely" — detection is unchanged.

## Verification — 2026-09-22 (Devin session)

- `cargo test --bin temote-mcp --all-features --locked` full parallel suite ×10 consecutive
  runs: zero `session_gc` / restore-plan / `protected_upgrade` failures (the only failures are
  the documented host-env set: `github_https_*`, `github_pr_tools`, `structured_git_remote_branch_delete`,
  `local_agent`/`managed_worktree` sandbox-helper tests — identical on clean `main`).
- Focused tests: `restore_plan_scan_still_fails_closed_on_a_malformed_plan` + the
  `session_gc_*`, `upgrade_failure_report_*`, `upgrade_plan` tests all pass.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo check --no-default-features --all-targets --locked`, `git diff --check` all clean.
