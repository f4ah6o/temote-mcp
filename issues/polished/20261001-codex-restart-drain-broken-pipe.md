# CI1: stabilize Codex restart-drain test on macOS without weakening drain semantics

Status: ready
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
Source tracker: `issues/closed/20260926-macos-ci-flakes-v3.md`
Observed test: `codex_app_server::tests::restart_does_not_start_replacement_until_old_turn_drained`

## 1. Problem

A macOS CI run failed in the Codex app-server restart-drain test with:

```text
called Result::unwrap() on an Err value: Broken pipe (os error 32)
```

The same SHA passed on immediate rerun. No production-code cause has been confirmed. The test still has to prove that a replacement session cannot begin before the old turn drains.

## 2. Goal

Make the restart-drain test deterministic on macOS while preserving the production invariant. Determine whether the broken pipe is an expected test-harness shutdown race or exposes a production lifecycle bug; change only the layer supported by evidence.

## 3. Fixed constraints

- Do not add unconditional retry behavior to production merely to hide the test failure.
- Do not weaken the assertion that replacement startup is fenced until the old turn is drained.
- Treat peer-close / writer ordering as a hypothesis until reproduced.
- Keep Linux behavior unchanged.
- If investigation proves the production path is affected, record that evidence before changing production semantics.

## 4. Implementation steps

1. Reproduce the focused test repeatedly on macOS and record failure frequency and the exact writer/read side that receives EPIPE.
2. Instrument or structure the fake child/test harness enough to identify shutdown ordering without changing the production contract.
3. If EPIPE is expected after the peer has deliberately closed, make the test harness tolerate only that specifically proven shutdown case.
4. If EPIPE occurs before the intended close/drain point, fix the lifecycle race instead and retain a regression that fails on the old behavior.
5. Run the focused test repeatedly after the change, then the normal Rust and CI gates.

## 5. Acceptance

- [ ] Repeated macOS runs of the focused test no longer fail with `Broken pipe`.
- [ ] The test still proves replacement startup cannot precede old-turn drain.
- [ ] The change is supported by a reproduced shutdown ordering, not a blanket retry.
- [ ] Linux behavior and existing restart/recovery tests remain unchanged.
- [ ] `cargo fmt --all -- --check` PASS.
- [ ] focused Codex app-server tests PASS.
- [ ] `cargo test` PASS.
- [ ] `cargo clippy --all-targets -- -D warnings` PASS.
- [ ] `cargo check --no-default-features --all-targets` PASS.
- [ ] `git diff --check` PASS.

## 6. Non-goals

- Reopening the session-metadata retention flake fixed by PR #66.
- Hiding unrelated flaky tests with retries.
- Changing production error handling without evidence that the production path is involved.
