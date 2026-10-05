# CI1: stabilize Codex restart-drain test on macOS without weakening drain semantics

Status: polished
Model: unknown
Created: 2026-10-01
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Make the macOS restart-drain test deterministic while preserving the old-turn drain fence.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Make the macOS restart-drain test deterministic while preserving the old-turn drain fence.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 6. Non-goals

- Reopening the session-metadata retention flake fixed by PR #66.
- Hiding unrelated flaky tests with retries.
- Changing production error handling without evidence that the production path is involved.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Preserved fixed contract: 3. Fixed constraints

- Do not add unconditional retry behavior to production merely to hide the test failure.
- Do not weaken the assertion that replacement startup is fenced until the old turn is drained.
- Treat peer-close / writer ordering as a hypothesis until reproduced.
- Keep Linux behavior unchanged.
- If investigation proves the production path is affected, record that evidence before changing production semantics.

## 受け入れ条件

Complete source criteria from “5. Acceptance” (unchecked items remain unverified):

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

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd gateway && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.

## 既存設計・履歴

> Historical Status: ready
Repository: `f4ah6o/temote-mcp`
> Historical Created: 2026-10-01 (Asia/Tokyo)
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
