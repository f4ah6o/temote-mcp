# Make agent-child lifecycle shutdown resilient to transient failures

Status: open
Model: GPT-6
Created: 2026-09-27
Updated: 2026-09-27
Branch: main

## 概要

Fix confirmed Codex and OpenCode child-runtime lifecycle bugs so uncertain parent-session probes cannot trigger shutdown, interrupted cleanup can be retried, and task records remain recoverable. Preserve generation ownership, canonical scope, and runtime-lease safety throughout cleanup and retry.

## 背景

The Codex and OpenCode server-backed child runtimes use `SessionInstance` ownership, lifecycle closing fences, runtime leases, and durable task stores. A read-only investigation confirmed related cleanup failures in `src/codex_app_server.rs`, `src/opencode_server.rs`, and `src/supervisor.rs`.

The related issue [`issues/done/20260927-opencode-sandboxed-shell.md`](../done/20260927-opencode-sandboxed-shell.md) covers shell permission for sandboxed local OpenCode runs. That capability/configuration change is separate from the server-backed child lifecycle bugs here.

## 問題

- Codex and OpenCode `wait_for_session_stop` treat metadata-read errors as inactivity and use `session_is_active(...).await.unwrap_or(false)`. A transient metadata or liveness-probe error can therefore start provider-child shutdown while the parent Temote session is still active.
- A lifecycle registry entry can remain `closing = true` indefinitely. Cleanup calls `finish_session_shutdown` only after draining and task finalization succeed; a drain timeout, task-store open failure, or finalization failure can return before clearing the fence. The watcher logs the failure but does not retry cleanup. Calls for that same owner then continue to fail as “session instance is closing.”
- `supervisor::remove_agent_sessions` propagates a Codex cleanup error with `?` before attempting OpenCode cleanup. This can skip OpenCode cleanup even when both provider instances were fenced for shutdown.
- Durable task metadata can remain non-terminal but become inaccessible through normal task APIs after the child runtime is gone while the same-owner lifecycle remains stuck closing.
- Devin produced a similar externally observed “session instance is closing” symptom. No equivalent Devin lifecycle implementation was found in this checkout, so its root cause is unverified and must be treated as a separate follow-up, not as a confirmed instance of these Codex/OpenCode bugs.

## 目標

Make child shutdown depend on confirmed parent inactivity or generation change, make cleanup safely retryable until it completes, attempt cleanup for every provider, and keep durable task state recoverable when a child runtime has exited.

## 対象外

- Proving or fixing the Devin symptom; investigate it separately if a corresponding implementation or reproduction becomes available.
- Changing OpenCode shell permissions or the local-agent sandbox capability described in `issues/done/20260927-opencode-sandboxed-shell.md`.
- Weakening `SessionInstance` generation checks, canonical scope validation, lifecycle fences, or runtime-lease ownership.

## 提案する方針

- Treat metadata and liveness probe errors as unknown. Retry and debounce observations; initiate watcher-driven shutdown only after confirmed inactivity or a confirmed session-generation change. Keep explicit supervisor shutdown authoritative.
- Make cleanup idempotent and retryable after drain, task-store-open, or finalization failures. Keep the closing fence in place until owner cleanup really completes, while allowing a retry for that same full `SessionInstance` to resume cleanup.
- Have `remove_agent_sessions` attempt Codex and OpenCode cleanup independently, then return or aggregate the collected errors.
- Preserve full `SessionInstance` ownership, canonical scope, and runtime leases across retries. Ensure stale children or task records cannot attach to a same-id replacement session.
- Keep unfinished durable task records discoverable or otherwise recoverable through the supported task lifecycle after the provider runtime exits.

## 受け入れ条件

- [ ] Transient metadata-read and liveness-probe errors do not shut down a provider child while its parent session remains active.
- [ ] Confirmed parent inactivity and confirmed generation replacement still trigger watcher cleanup; explicit supervisor shutdown remains authoritative.
- [ ] Drain timeout, task-store-open failure, and task-finalization failure retain the closing fence and allow a later idempotent retry to finish cleanup.
- [ ] Non-terminal durable tasks remain recoverable through the supported task lifecycle after their child runtime exits and cleanup is retried.
- [ ] Same-id replacement sessions remain isolated by full `SessionInstance` ownership, canonical scope, and runtime-lease checks.
- [ ] Supervisor cleanup attempts both Codex and OpenCode even when Codex cleanup fails, then reports cleanup errors.
- [ ] Devin's similar symptom is documented as unverified and tracked separately from the confirmed Codex/OpenCode defects.

## テスト計画

- Add focused watcher tests for transient metadata/probe errors, confirmed stop, and confirmed generation change.
- Add cleanup failure-then-retry tests for drain timeout, task-store open/finalization failure, and task recoverability.
- Add a same-id replacement test proving old child/runtime leases cannot affect the replacement owner.
- Add a supervisor test where Codex cleanup fails and OpenCode cleanup is still attempted, with errors returned afterward.
- Run the focused Codex, OpenCode, and supervisor Rust test filters, then the repository's required Rust checks for the implementation change.

## リスク

Treating a failed probe as unknown can delay watcher-driven shutdown during a prolonged metadata or liveness outage. Keep retries bounded per attempt and retain explicit supervisor shutdown so confirmed lifecycle operations can still drive cleanup. Cleanup retries must remain fenced to the original owner and must not admit work for a replacement session.

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Make delegated Codex and OpenCode child shutdown resilient to transient session probes and retryable cleanup failures.

## 注記

- 2026-09-27: Read-only investigation confirmed the Codex/OpenCode and supervisor findings above. Devin's root cause remains unverified.
