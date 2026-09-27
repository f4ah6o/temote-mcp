# Make agent-child lifecycle shutdown resilient to transient failures

Status: open
Model: GPT-6
Created: 2026-09-27
Updated: 2026-09-27
Branch: main

## 概要

Fix confirmed Codex, OpenCode, and Devin ACP child-runtime lifecycle bugs so uncertain parent-session probes cannot trigger shutdown, interrupted cleanup can be retried, and task records remain recoverable. Preserve generation ownership, canonical scope, and runtime-lease safety throughout cleanup and retry.

## 背景

The Codex and OpenCode server-backed child runtimes use `SessionInstance` ownership, lifecycle closing fences, runtime leases, and durable task stores. A read-only investigation confirmed related cleanup failures in `src/codex_app_server.rs`, `src/opencode_server.rs`, `src/devin_acp.rs`, and `src/supervisor.rs`.

The related issue [`issues/open/20260927-opencode-checkout-command-execution-capability.md`](20260927-opencode-checkout-command-execution-capability.md) owns execution-time checkout/cwd/command capability for OpenCode implementation tasks. Current `opencode serve` task configuration explicitly denies the shell capability, so that provisioning/capability gap is real but separate from the child lifecycle bugs here.

## 問題

- Codex, OpenCode, and Devin ACP `wait_for_session_stop` all treat metadata-read errors as inactivity and use `session_is_active(...).await.unwrap_or(false)`. A transient metadata or liveness-probe error can therefore start provider-child shutdown while the parent Temote session is still active.
- A lifecycle registry entry can remain `closing = true` indefinitely. Cleanup calls `finish_session_shutdown` only after draining and task finalization succeed; a drain timeout, task-store open failure, or finalization failure can return before clearing the fence. The watcher logs the failure but does not retry cleanup. Calls for that same owner then continue to fail as “session instance is closing.”
- `supervisor::remove_agent_sessions` propagates cleanup errors with `?` between backends. A Codex failure skips OpenCode and Devin ACP cleanup; an OpenCode failure skips Devin ACP cleanup, even when those provider instances were already fenced for shutdown.
- Durable task metadata can remain non-terminal but become inaccessible through normal task APIs after the child runtime is gone while the same-owner lifecycle remains stuck closing.
- Devin ACP contains the same `wait_for_session_stop` liveness/error-collapse pattern plus its own lifecycle registry and shutdown fence, so the source-level defect is confirmed there as well. The previously observed Devin “session instance is closing” incident is still not proven to have been caused by this exact path; causal attribution of that historical incident remains a live/reproduction question rather than a reason to exclude Devin ACP from the fix.

## 目標

Make child shutdown depend on confirmed parent inactivity or generation change, make cleanup safely retryable until it completes, attempt cleanup for every provider, and keep durable task state recoverable when a child runtime has exited.

## 対象外

- Proving that the previously observed Devin symptom was caused by this exact mechanism. The implementation scope does include fixing the confirmed Devin ACP parity defect in the watcher/lifecycle code.
- Changing OpenCode checkout/shell capability or implementation-task provisioning; that remains owned by `issues/open/20260927-opencode-checkout-command-execution-capability.md`.
- Weakening `SessionInstance` generation checks, canonical scope validation, lifecycle fences, or runtime-lease ownership.

## 提案する方針

- Treat metadata and liveness probe errors as unknown. Retry and debounce observations; initiate watcher-driven shutdown only after confirmed inactivity or a confirmed session-generation change. Keep explicit supervisor shutdown authoritative.
- Make cleanup idempotent and retryable after drain, task-store-open, or finalization failures. Keep the closing fence in place until owner cleanup really completes, while allowing a retry for that same full `SessionInstance` to resume cleanup.
- Have `remove_agent_sessions` attempt Codex, OpenCode, and Devin ACP cleanup independently, then return or aggregate the collected errors.
- Preserve full `SessionInstance` ownership, canonical scope, and runtime leases across retries. Ensure stale children or task records cannot attach to a same-id replacement session.
- Keep unfinished durable task records discoverable or otherwise recoverable through the supported task lifecycle after the provider runtime exits.

## 受け入れ条件

- [ ] Transient metadata-read and liveness-probe errors do not shut down Codex, OpenCode, or Devin ACP children while their parent session remains active.
- [ ] Confirmed parent inactivity and confirmed generation replacement still trigger watcher cleanup; explicit supervisor shutdown remains authoritative.
- [ ] Drain timeout, task-store-open failure, and task-finalization failure retain the closing fence and allow a later idempotent retry to finish cleanup.
- [ ] Non-terminal durable tasks remain recoverable through the supported task lifecycle after their child runtime exits and cleanup is retried.
- [ ] Same-id replacement sessions remain isolated by full `SessionInstance` ownership, canonical scope, and runtime-lease checks.
- [ ] Supervisor cleanup attempts Codex, OpenCode, and Devin ACP independently; failure in an earlier backend does not skip cleanup of later backends, and collected cleanup errors are reported afterward.
- [ ] The historical externally observed Devin symptom remains documented as causally unverified even though the matching Devin ACP source-level lifecycle defect is fixed.

## テスト計画

- Add focused Codex, OpenCode, and Devin ACP watcher tests for transient metadata/probe errors, confirmed stop, and confirmed generation change.
- Add cleanup failure-then-retry tests for drain timeout, task-store open/finalization failure, and task recoverability.
- Add a same-id replacement test proving old child/runtime leases cannot affect the replacement owner.
- Add supervisor tests proving a Codex cleanup failure does not skip OpenCode/Devin ACP and an OpenCode cleanup failure does not skip Devin ACP, with errors returned afterward.
- Run the focused Codex, OpenCode, Devin ACP, and supervisor Rust test filters, then the repository's required Rust checks for the implementation change.

## リスク

Treating a failed probe as unknown can delay watcher-driven shutdown during a prolonged metadata or liveness outage. Keep retries bounded per attempt and retain explicit supervisor shutdown so confirmed lifecycle operations can still drive cleanup. Cleanup retries must remain fenced to the original owner and must not admit work for a replacement session.

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Make delegated Codex, OpenCode, and Devin ACP child shutdown resilient to transient session probes and retryable cleanup failures.

## 注記

- 2026-09-27: Follow-up current-main review confirmed that Devin ACP has the same `wait_for_session_stop` error-collapse pattern and lifecycle registry, and that supervisor early-return ordering can skip Devin cleanup. The source-level Devin ACP defect is therefore in scope; only the causal link to the previously observed external Devin symptom remains unverified.
