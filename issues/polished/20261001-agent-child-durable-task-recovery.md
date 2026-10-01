# LC3: preserve durable task recovery after provider runtime loss

Status: ready
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
Parent/source: `issues/closed/20260927-agent-child-lifecycle-cleanup.md`
Related: `issues/polished/20261001-agent-child-cleanup-retry-and-provider-aggregation.md`

## 1. Goal

A non-terminal Codex/OpenCode/Devin ACP task record must remain discoverable and recoverable through the supported task lifecycle when its provider child runtime exits or cleanup is retried. Runtime loss must not turn durable task metadata into an inaccessible orphan.

## 2. Confirmed risk

The source investigation found that non-terminal durable records can outlive the child runtime while the same owner remains in a closing lifecycle state. Normal task APIs can then become unusable even though the task record still exists.

## 3. Fixed design

- Durable task record remains the retained source for task identity/status after runtime loss.
- `task_list` must continue to rediscover the retained owned task.
- A read/recovery path must distinguish:
  - runtime can be safely reconstructed/reconciled,
  - task is terminal after reconciliation,
  - runtime state is uncertain and requires reconciliation,
  - original owner is closing and recovery is temporarily unavailable.
- Do not fabricate a completed/failed state merely because the process is gone.
- Do not attach an old task/runtime to a same-id replacement session; validate full `SessionInstance`, canonical scope, and generation before recovery.
- Recovery must not duplicate the original start/control side effect.

## 4. Scope

Implement the smallest common behavior needed by Codex/OpenCode/Devin ACP retained task APIs. Avoid adding a new generic execution API in this packet.

## 5. Acceptance

- [ ] A retained non-terminal task remains visible through the supported list/lookup surface after provider runtime loss.
- [ ] Safe runtime reconstruction/reconciliation can recover the original task without creating a second task or turn.
- [ ] An uncertain state is reported explicitly instead of being guessed terminal.
- [ ] Closing-owner state is distinguishable from missing-task state.
- [ ] Same-id replacement sessions cannot access or adopt the prior instance's task/runtime.
- [ ] Operation receipt/replay protection remains intact across recovery.
- [ ] Terminal scoped evidence behavior remains unchanged.

## 6. Tests

Add focused crash/runtime-loss fixtures for Codex, OpenCode, and Devin ACP, including same-id replacement isolation, then run:

- [ ] focused retained-task recovery tests PASS
- [ ] `cargo fmt --all -- --check` PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## 7. Non-goals

- General task waiting/polling optimization.
- New cross-backend task migration.
- Attributing the historical Devin incident to the verified equivalent source defect without incident evidence.
