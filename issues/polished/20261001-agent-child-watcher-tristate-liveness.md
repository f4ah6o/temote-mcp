# LC1: make agent-child watcher liveness tri-state and fail safe on probe errors

Status: ready
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
Parent/source: `issues/closed/20260927-agent-child-lifecycle-cleanup.md`

## 1. Goal

Codex, OpenCode, and Devin ACP child-runtime watchers must initiate provider-child shutdown only from confirmed parent-session inactivity or confirmed generation replacement. Metadata/liveness probe errors are `unknown`, never evidence that the parent is inactive.

## 2. Confirmed defect

The current watcher path uses a boolean active check and collapses errors with behavior equivalent to `session_is_active(...).await.unwrap_or(false)`. A transient metadata or liveness error can therefore be interpreted as inactivity and begin child shutdown while the parent session is still active.

## 3. Fixed design

Represent watcher observation explicitly:

```text
active
inactive
replaced
unknown(error)
```

- `active`: retain the current child.
- `inactive`: watcher-driven cleanup may start after the existing bounded/debounce policy.
- `replaced`: old-generation cleanup may start; preserve full `SessionInstance` fencing.
- `unknown`: do not begin shutdown. Retry later and retain explicit supervisor shutdown as the authoritative escape path.
- Do not weaken canonical scope, generation, or runtime-lease checks.

Apply the same semantics to Codex, OpenCode, and Devin ACP watcher paths rather than fixing only one adapter.

## 4. Scope

Expected code surface:

- `src/codex_app_server.rs`
- `src/opencode_server.rs`
- `src/devin_acp.rs`
- shared session/liveness helper only if necessary for one common tri-state result

Devin ACP has the same metadata/error-collapse watcher path in `wait_for_session_stop`; include that source defect. Attribution of the historical Devin incident remains unverified.

## 5. Acceptance

- [ ] A transient metadata-read error does not stop an active Codex child.
- [ ] A transient metadata-read error does not stop an active OpenCode child.
- [ ] A transient metadata-read error does not stop an active Devin ACP child.
- [ ] A transient liveness-probe error is reported/treated as unknown, not inactive.
- [ ] Confirmed parent inactivity still triggers watcher cleanup.
- [ ] Confirmed same-id generation replacement still fences and cleans the old child.
- [ ] Explicit supervisor shutdown remains authoritative.
- [ ] Existing `SessionInstance`, canonical-scope, and runtime-lease isolation tests remain green.

## 6. Tests

Add deterministic watcher tests for active / inactive / replaced / unknown for all three provider adapters, then run:

- [ ] focused Codex watcher tests PASS
- [ ] focused OpenCode watcher tests PASS
- [ ] focused Devin ACP watcher tests PASS
- [ ] `cargo fmt --all -- --check` PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## 7. Non-goals

- Retryable cleanup after a cleanup operation has already started (LC2).
- Durable task recovery after the runtime is already gone (LC3).
- Claiming that the historical Devin incident was caused by this source defect without incident evidence.
