# Isolate MCP transport failures from task execution and observation

Status: implementation complete; production rollout and live acceptance pending
Created: 2026-10-05 (Asia/Tokyo)
Baseline: `02d8285ee4abf2b506ade8dc014cb047362798eb`

## 1. Problem

Remote MCP use remains difficult when a slow tool call blocks unrelated state
queries, or a transient backend observation is reported as an execution result.
This packet repairs those boundaries without changing the public tool inventory,
authorization model, or standard MCP compatibility contract.

The following mechanisms were verified in the baseline source and deterministic
gateway probes. They are concrete defects, not a claim that every reported
production failure has the same cause.

1. The host agent polls, executes one RPC, and uploads its response before polling
   again. A slow backend call therefore blocks other calls and lease renewal.
2. Gateway dispatch expires after 35 seconds. A late upload returns
   `409 stale_request`, even while the host lease remains active. The agent treats
   every response-upload `409` as a replaced generation, reconnecting and failing
   other pending calls with `host_replaced`.
3. Explicit `host_info` and `session_list({host_id})` enumerate every host's status,
   so an unrelated unresponsive host can delay a fully qualified request.
4. OpenCode reconciliation converts failed status/message/interaction reads into
   empty values. These values can incorrectly derive a terminal state from an
   incomplete observation.
5. Unchanged reconciliation and repeated identical errors can advance retained
   task revisions. This defeats `after_revision` and can keep refreshing the
   OpenCode prompt-admission grace period.
6. Overlapping task reads and controls can apply an observation from an older
   turn to a newer retained state. Revision checks alone do not cover a control
   that is accepted but has not finished sending and applying its result.

## 2. Implementation boundaries

1. Separate the host connection/poll loop from bounded in-flight request
   execution. Keep one shared concurrency budget across reconnects, preserve the
   original generation on responses, and never replay an accepted operation.
2. Distinguish an expired request from a replaced connection. Keep lease renewal
   available when execution slots are occupied. Negotiate the internal host
   capability so older gateways remain compatible.
3. Query only the selected host for explicit discovery. Preserve fail-closed
   ownership checks for unqualified routing.
4. Treat backend reads as observations with a shared time budget. Derive execution
   state only from a complete observation; preserve uncertainty on failure.
5. Advance a retained task revision only when its semantic record changes. Keep
   execution, verification, and delivery separate.
6. Fence successful and failed observations against the record they read. Defer
   reconciliation while a task control is actively in flight; release that
   protection when the operation exits so crash recovery remains possible.
   Retain cross-process task ownership and private lock-file boundaries.

## 3. Verification coverage

The regression suite covers these boundaries:

- A slow RPC does not prevent a second RPC from completing.
- Saturated execution slots do not prevent host lease renewal.
- A late response does not replace an otherwise healthy host generation.
- A real generation replacement fences new dispatch admission and old responses while retaining already-started local dispatches.
- Reconnect never duplicates an accepted operation or exceeds the concurrency budget.
- Explicit host lookup does not query an unrelated host; unqualified lookup remains fail-closed.
- Incomplete OpenCode observations do not fabricate failed/completed execution.
- Unchanged observations and identical read errors preserve retained revisions.
- Real execution/error/interaction transitions still invalidate the cursor.
- A stale observation cannot overwrite a newer execution/control revision or attach its interactions/evidence to that newer state.
- Reads defer while a control is being admitted/sent/applied; one control guard cannot release another control's protection.

Formatting, relevant Rust tests, clippy, the local-only build, gateway tests, and
diff checks remain required gates. Validation commands and actual outcomes are
recorded in the PR. Host-only gates
and live production acceptance must remain explicitly distinct from local tests.

## 4. Related work

- [Development harness restructure](20260924-temote-development-harness-restructure.md)
- [Bounded delegated-task wait](20260927-bounded-wait-for-delegated-tasks.md)
- [BW1 semantic task revisions](../polished/20261001-task-get-semantic-revision-stability.md)

This packet does not add long polling, `wait_ms`, new task APIs, events, or a
different agent backend. It does not claim to complete the wider BW1 evidence and
summary cursor contract. A transport failure remains distinct from a verified
task failure, and a completed execution remains distinct from verification PASS.
