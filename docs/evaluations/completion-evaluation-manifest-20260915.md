# Completion evaluation manifest (2026-09-15, Asia/Tokyo)

Status: frozen before evaluated solutions were inspected; execution in progress.

## Purpose and immutable boundary

This manifest defines the completion evaluation authorized for the 2026-09-15 Temote MCP work. The task statements and acceptance criteria below are frozen before any evaluated implementation is inspected. Each task starts from one exact common prerequisite commit across all three arms. A later prerequisite commit may be used only for a task that depends on earlier accepted production work; it is recorded before that task's first arm starts and is not changed between arms.

The repository-wide starting baseline is `0ee1db7e2e81a54faca6a4eb8d741e33702376d4`. The independently reviewed HTTP fix `232b5f1` was accepted before this evaluation lane began and is recorded as prerequisite evidence, not as an evaluated answer visible to this lane before T01 was frozen.

The evaluation does not test or add an OpenCode persistent server, attach mode, automatic resume, or background lifecycle. Its isolated arms do not change a production deployment, installed host runtime, global agent configuration, or host authentication file. A separate upgrade-process test did stop the original ingress; the user later approved ending 23 sessions and restarting the host supervisor and ingress. That incident and recovery are outside the evaluated arm outcomes and are recorded in the results document without changing the frozen task criteria.

## Execution arms

Each task is attempted in three independent worktrees created from the task's recorded common base:

| Arm | Runtime | Requested model / effort | Invocation class |
| --- | --- | --- | --- |
| A | Codex direct worker | `gpt-5.6-sol` / `high` | native Codex task worker using Temote source operations where available, constrained to the assigned worktree and frozen prompt |
| B | Temote Codex delegation adapter over Codex CLI 0.153.4 | `gpt-5.6-luna` / `max` | one-shot `temote-mcp delegate --backend codex`, which owns the `codex exec` contract |
| C | Temote MCP app-server adapter over Codex 0.153.4 | `gpt-5.6-luna` / `max` | `codex_task_start` followed by bounded `codex_task_get` reconciliation |

Invocation order rotates to reduce order bias: `ABC`, `BCA`, `CAB`, repeating by task. Arms never share a worktree or a previous arm's patch. Exactly one result may be selected for production integration. No comparison patch is imported without coordination with the implementation owner.

All arms receive the same task-specific text below plus this shared instruction:

> Work only in the assigned worktree at the supplied common base. Read its AGENTS.md and the relevant current code or issue. Implement the requested task completely, without unrelated changes or secret exposure. Run focused meaningful tests and the narrow required checks. Do not push, change package versions, inspect another evaluation arm, or use another arm's result. At completion, report changed files, checks, unresolved items, requested model/effort, and any actually observed model/effort or usage. Do not infer monetary cost from token or byte counts.

## Frozen tasks

### T01 — HTTP managed-session visibility and ordering

Ensure a managed session created through the selected HTTP backend appears immediately in `session_list`. Owned active sessions must precede retained stopped history. The stdio/`LocalControl` fallback must remain bounded and read-only. Add focused regression coverage. Do not change unrelated HTTP, session, or deployment behavior.

Initial common base: `0ee1db7e2e81a54faca6a4eb8d741e33702376d4`. Independently accepted implementation prerequisite/result reference: `232b5f1` (review approved before evaluated execution).

### T02 — Gateway target preflight CLI contract

Implement the gateway preflight behavior for a configuration with valid `--hostname`, `workers_dev=false`, and no target: stdout is one JSON result whose overall status and `target.status` are `target_missing`; stderr is empty; exit status is 1. Supplying both target flags exits 2. The command must not mutate remote state or configuration and must not disclose a target. Add focused CLI tests.

Initial common base: `0ee1db7e2e81a54faca6a4eb8d741e33702376d4`.

### T03 — Strict bounded activity-event decoder

Add a strict, bounded decoder for the existing S01 activity wire contract. A valid encoded event must round-trip. Reject missing required fields, unknown or duplicate keys, invalid enum/schema values, oversized input, and forbidden control characters with fixed errors. Preserve the existing wire format and add deterministic coverage for every listed class.

Initial common base: `0ee1db7e2e81a54faca6a4eb8d741e33702376d4`.

### T04 — Pure activity-event renderer

Add a pure one-line renderer that accepts an activity event and already-supplied local-time text. Preserve the full identity internally while shortening only its display. Sanitize control characters. Add stable golden coverage for started, waiting, and terminal states. Do not add clock, terminal, I/O, or transport behavior to the renderer.

Initial common base: `0ee1db7e2e81a54faca6a4eb8d741e33702376d4`.

### T05 — Instance-bound activity ingress

Add instance-bound activity update ingress using the existing S03 boundary. Accept a valid update for the current instance. Reject stale, retired, unknown, and oversized updates. Keep the acknowledgement bounded. The ingress must not trigger approval decisions or session operations. Add deterministic contract tests.

Common base requirement: the accepted S03 prerequisite only. Frozen common base before arm 1: `0ee1db7e2e81a54faca6a4eb8d741e33702376d4`.

### T06 — Process-wide activity producer

Add a typed process-wide producer with a queue capacity of 256 and ordered sequential delivery. The combined connect, write, and acknowledgement sequence has one shared one-second deadline. Do not retry, block caller I/O on delivery, or let delivery failure alter the originating operation. Add deterministic ordering, capacity, timeout, and failure-isolation tests.

Common base requirement: the accepted T05/S05 prerequisite. Frozen common base before arm 1: `523435b42ad0ccd6c2ccaade5374ac4871f25433`.

### T07 — Job terminal-race tests

Add meaningful deterministic tests to the implemented job-scope path for both natural-finish-first and stop-first races. Each path must emit exactly one terminal activity state while preserving the existing job response and evidence. Do not use scheduler sleeps as synchronization and do not alter unrelated job behavior.

Common base requirement: the accepted S12 job-scope implementation. Frozen common base before arm 1: `a24a40029e48204718803843181e2a1684b23ea0` (reviewed with no remaining findings and merged unchanged into the integration line at `82b3bf3`).

### T08 — Upgrade response-flush transport tests

Add real transport-boundary tests to the implemented upgrade path for a fully flushed accepted response and for write, reset, and peer-drop failures. A disconnect after a proven flush may commit; an incomplete response or an observed write/reset failure before flush must never permit the destructive commit point. Use deterministic synchronization with no arbitrary sleeps and preserve existing upgrade behavior.

Common base requirement: the accepted upgrade implementation. Frozen common base before arm 1: `8ebaf598d61704447ecd73e4c4cfdd6e50f37b2d` (merged unchanged into the current integration line at `40cda15`).

### T09 — English activity operator documentation

Document the implemented activity operator interface in English: actual CLI syntax, defaults, and filters; local-only and best-effort in-memory behavior; caps and gap semantics; and signal plus EOF/BrokenPipe/reconnect behavior. State that observation does not mutate the watched task. Keep user-facing documentation aligned with the implementation and concise.

Common base requirement: accepted S15 activity code; record its exact commit before arm 1.

### T10 — Japanese activity operator documentation

Document the same implemented activity operator interface in Japanese: actual CLI syntax, defaults, and filters; local-only and best-effort in-memory behavior; caps and gap semantics; and signal plus EOF/BrokenPipe/reconnect behavior. State that observation does not mutate the watched task. Keep terminology consistent with the English documentation and current product wording.

Common base requirement: the same accepted S15 commit used for T09; record its exact commit before arm 1.

## Evaluation and selection

Each arm is recorded with its exact base, worktree, invocation order, requested and observed model/effort, start/end and wall time, exit/final state, retry count, human or parent intervention, changed paths, checks, and bounded usage when the runtime exposes it. Missing usage remains `unknown`. Backend-specific usage is not treated as directly comparable and bytes are never converted into cost. Actual API cost is recorded only if an official verified rate and billable units are both available; otherwise cost remains unknown.

Review uses the frozen acceptance criteria, focused test quality, correctness, scope, safety invariants, maintainability, and completion without manual repair. A runtime failure after an arm starts remains in the attempted-arm denominator with its distinct reason. An arm that cannot start because its common prerequisite is unavailable is recorded separately as unattempted. Neither category is excluded to improve a success metric. If direct, delegate-exec, and app-server permissions, tools, or observability cannot be made equivalent, the result is labeled non-comparable for that dimension.

Manifest correction before the first evaluated arm: T06 was clarified as one combined one-second deadline; arm B was corrected to the Temote-owned Codex delegation adapter; T08 was clarified to allow commit after proven flush; attempted runtime failures were explicitly retained in denominators. These changes transcribe the accepted plan and were frozen before any arm ran.

## Runtime ledger before task execution

Evaluation root: `/tmp/temote-completion-eval-20260915` (outside the repository and configured Temote roots).

| Component | Host installation (unchanged) | Isolated evaluation runtime | Provenance / result |
| --- | --- | --- | --- |
| Temote MCP | `2026.9.7` | debug build from repository baseline (`2026.8.0` package baseline) | production installation unchanged |
| Vite+ | `0.3.1` | host Vite+ launcher used only for the dedicated Linux acceptance | installation unchanged |
| Codex | Vite+ `0.147.0` | npm `@openai/codex@0.153.4` | npm integrity recorded; upstream `github.com/openai/codex`; version exact |
| OpenCode | `1.17.10` | npm `opencode-ai@1.18.30` | npm integrity recorded; version exact |

The isolated Codex home and OpenCode data/config roots contain mode-0600 copies of only the needed existing authentication/account files. Their contents were never printed or inspected. Temote state uses an isolated `XDG_STATE_HOME`; child PATH selects only the pinned evaluation runtime where required. No production listener, socket, state directory, deployment, or global configuration is reused for mutations.

Pinned Codex 0.153.4 app-server `model/list` advertised `gpt-5.6-luna` with `max` and `gpt-5.6-sol` with `high`. Direct one-shot read-only auth/model smokes for Luna/max and Sol/high both passed. Pinned OpenCode 1.18.30 diagnostics succeeded and listed 96 models. A listed `opencode-go/deepseek-v4-flash` call was denied by the provider's regional opt-in requirement; listed OpenCode-hosted GPT models were denied because the isolated account has no payment method. These are entitlement results, not Temote adapter failures. A model-list-only result is not treated as live delegation acceptance.

Official package metadata captured before installation:

- `@openai/codex@0.153.4`: `sha512-wbHDmit7S/YvBGVX1DQmk13xtWblZ2cApeJ/pB7xDZ10Cna+DZc5ij7f0F4OxdsXN4FW1oLT48OpogUI1+8Y2w==`.
- `opencode-ai@1.18.30`: `sha512-oLcOLQE4XzDKy6T5L5d1RdVJvXHXwVlD4hRF5V317JbUQorrl2EyDdGZk5kbgv675J9FXp8usg92MZbEWhh6gQ==`.

## Remaining execution units at freeze time

- Linux Vite+ `local_agent_run`: read-only, then bounded workspace-write, plus protected metadata and sibling-path checks.
- Real Codex app-server: status/model list, a no-change task, task reconciliation, and a typed control path if the task state permits it.
- OpenCode: try only already-listed, bounded one-shot candidates; record provider entitlement precisely if no model can execute. Do not add persistent server/attach behavior.
- Live acceptance matrix: use existing read-only configured connections where available; do not change a production deployment.
- Completion comparison: 10 tasks × 3 arms, gated by each task's exact common-base prerequisite.
