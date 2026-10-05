# Backend task capabilities

Temote MCP accepts typed tasks inside an authorized session. The task tools do not accept commands, executable paths, environment blocks, or network policy. A report describes the backend's execution result; `completed` does not imply verification passed or delivery occurred.

| Backend | Structured report | Source shown in task view | Limit |
| --- | --- | --- | --- |
| Codex app-server 0.157.1 / 0.160.0 | `turn/start.outputSchema` constrains the final assistant message; Temote validates the parsed object | `native_structured_output` | Report JSON at most 8 KiB |
| OpenCode serve v2.0.11 | V2 prompt `format: {type: "json_schema", schema}`; Temote reads `structured_output` from the assistant message and validates it | `native_structured_output` | Report JSON at most 8 KiB |
| OpenCode legacy SDK | No verified native format path; bounded final-message JSON extraction | `final_message_compat` | Report JSON at most 8 KiB |
| Devin ACP | No verified ACP schema-constrained prompt/result path; bounded final-message JSON extraction | `final_message_compat` | Report JSON at most 8 KiB |
| Devin Cloud | `structured_output_schema` with required structured output | `native_structured_output` | Existing Cloud response and report bounds |

The shared report contract has two compatibility profiles. Delegation evidence files require exactly ten fields, including requested and observed model and effort. Task reports require `status` and `summary`, allow optional arrays, and retain the historical 8 KiB serialized limit. Devin Cloud's serialized wire schema is unchanged. A malformed native result is reported as malformed or missing; final-message text cannot replace it.

The Codex `outputSchema` field was checked against the installed 0.157.1 `v2/TurnStartParams` schema. A no-tool canary on installed 0.160.0 returned a valid native structured report; this does not establish every model/provider or legacy parity. The OpenCode request and `structured_output` response fields follow the [OpenCode SDK structured output contract](https://opencode.ai/docs/sdk/#structured-output); model/provider acceptance still needs a live host check.

Codex keeps a completed or interrupted task's ID when a new `steer` opens another turn. The new turn gets a fresh generation and turn ID; the predecessor is fenced during reconciliation. A running turn still uses `turn/steer`. The operation ID remains the durable replay key. The host may select a Codex executable with `TEMOTE_CODEX_BINARY` (the old `TEMOTE_MCP_CODEX_BINARY` is a read-only alias); task arguments cannot select one.

OpenCode checks the serve executable and writable session scope before accepting a task. An absent checkout is advisory unless the task has a repository/workspace requirement. Its native shell permission remains denied. Opted-in managed tasks can use the private typed [workspace command bridge](opencode-scoped-workspace.md); ordinary tasks retain the shell denial. Provider-backed managed build/test acceptance is recorded separately as NOT RUN. The preflight does not create or modify a checkout.

Provider-backed structured output and local socket/process lifecycle tests require an unsandboxed host or CI. The local deterministic fixtures cover schema shape, parsing, and idempotent task behavior; they do not certify provider support for every model.

## Codex conversation continuation

`codex_task_start` can optionally set `continuation` to `{"type":"previous_task","task_id":"<UUID>"}`. The source must be a retained, terminal Codex task in the exact active session and canonical scope, with no in-flight operation and a retained thread. Temote claims one successor, retires the source runtime, then starts a fresh Codex app-server and resumes the source thread before opening the successor's first turn. The successor has its own task ID, receipts, execution, verification, and delivery. Its task view reports `continued_from_task_id`; the source reports `continued_by_task_id`. Omit `continuation` or use `{"type":"new"}` to start a new thread. The local CLI exposes `--continue-from-task <UUID>` only for Codex starts. A claimed or unavailable source fails closed; retry an uncertain start with the same `operation_id` and exact request.

## Legacy entrypoint parity

The one-shot `delegate`, `codex exec`, and `opencode run` compatibility paths
remain available. Installing the canonical `temote` binary does not remove the
vendor Codex or OpenCode binaries needed by app-server and serve. Prefer
`temote task` for retained task receipts, scoped evidence and typed controls.
No compatibility path is removed based only on a status probe.

The 2026-10-06 integration used retained server tasks and protocol fixtures.
A credentialed, paired one-shot/server canary with identical inputs was not
performed; the following are independent parity gates. Fixture results verify
protocol behavior and do not replace these measurements.

| Parity dimension | Server protocol validation | Paired live measurement |
| --- | --- | --- |
| Native structured report | PASS: profile and native/malformed fixtures | NOT RUN: no paired provider turn |
| Usage | PASS: semantic usage/revision fixtures | NOT RUN: no paired billing measurement |
| Requested/observed model | PASS: selectors and reported identity fixtures | NOT RUN: no paired provider model observation |
| Effort/variant | PASS: selector and capability fixtures | NOT RUN: no paired provider effort observation |
| Denial/approval | PASS: shared ask/agent admission fixtures | NOT RUN: no paired interactive denial |
| Interrupt | PASS: typed control and generation fixtures | NOT RUN: no paired live turn interruption |
| Orphan-free shutdown | PASS: process/runtime lease and cleanup fixtures | NOT RUN: no paired one-shot/server shutdown |

Devin provider entitlements and fresh Fabric Link runtime injection were
unavailable. A live Codex canary in the integration evaluation is a server
acceptance result, not a paired parity result. Keep the compatibility period
until the supported caller inventory and these measurements justify removal.
