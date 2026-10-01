# LC2: make agent-child cleanup retryable and attempt every provider

Status: ready
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
Parent/source: `issues/closed/20260927-agent-child-lifecycle-cleanup.md`
Depends on: LC1 is independent; either packet may land first if tests preserve the same lifecycle contract.

## 1. Goal

Once provider-child shutdown begins, cleanup must be safely retryable for the same full `SessionInstance`, and supervisor removal must attempt every provider even if one provider cleanup fails.

## 2. Confirmed defects

- A lifecycle entry can remain `closing = true` indefinitely when drain, task-store open, or finalization fails before `finish_session_shutdown`.
- The watcher logs that failure but does not establish a retry path.
- Calls for the same owner then continue failing as "session instance is closing".
- `supervisor::remove_agent_sessions` can return after Codex cleanup fails and skip OpenCode cleanup.

## 3. Fixed design

- Keep the closing fence after partial cleanup failure; do not reopen the old owner to new work.
- Permit a later cleanup call for the **same full SessionInstance** to resume the idempotent cleanup steps.
- Cleanup for a different generation remains rejected/fenced.
- Mark cleanup complete only after provider drain/finalization succeeds.
- In supervisor removal, attempt Codex and OpenCode cleanup independently and aggregate/return the resulting errors after both attempts.
- Do not convert uncertain cleanup into success.
- Preserve runtime-lease ownership and canonical-scope checks on every retry.

## 4. Scope

Expected code surface:

- provider lifecycle/cleanup code in `src/codex_app_server.rs`
- provider lifecycle/cleanup code in `src/opencode_server.rs`
- `src/supervisor.rs::remove_agent_sessions`
- focused lifecycle tests

## 5. Acceptance

- [ ] A drain timeout leaves the original owner fenced and a later same-owner cleanup retry can finish.
- [ ] A task-store-open/finalization error leaves the original owner fenced and retryable.
- [ ] A replacement `SessionInstance` cannot take over the old cleanup attempt or lease.
- [ ] Successful retry clears the closing lifecycle state exactly once.
- [ ] Supervisor removal attempts OpenCode cleanup even when Codex cleanup fails.
- [ ] Multiple provider errors are retained/returned without silently dropping the later provider's result.
- [ ] Repeated cleanup is idempotent after completion.

## 6. Tests

- [ ] drain failure → retry → success
- [ ] task-store/finalization failure → retry → success
- [ ] same-id replacement cannot retry old-owner cleanup
- [ ] Codex cleanup failure still attempts OpenCode cleanup
- [ ] `cargo fmt --all -- --check` PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## 7. Non-goals

- Treating probe errors as inactivity (LC1).
- Defining task recovery semantics once a child runtime is gone (LC3).
- Weakening the closing fence to make retries easier.
