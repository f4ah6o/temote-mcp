---
name: temote-mcp
description: Delegate local-machine work to coding agents through Temote MCP sessions. Use when the user mentions Temote MCP or temote-mcp, asks an agent to work on a local repository/session through Temote, supplies a Temote session ID, or when tools such as session_list, session_info, codex_task_start, opencode_task_start, devin_task_start, or devin_cloud_task_start are available.
license: MIT AND Apache-2.0
compatibility: Requires an MCP connection to Temote MCP. A serve endpoint may create normal sessions from host-configured named roots; otherwise tools using session_id require an existing local session.
metadata:
  author: f4ah6o
---

# Use Temote MCP effectively

Temote MCP exposes a user's local machine through explicit sessions and delegates machine operations to a coding agent running on that machine (Codex app-server, `opencode serve` via the OpenCode SDK, or Devin via ACP or the Devin Cloud API). Temote does not execute files, commands, Git, or host integrations directly: pick the session and backend, hand the task to the local agent, and read results through the task/evidence API.

Treat the selected session as the source of truth for its working directory, permission mode (`ask`, `agent`, or `yolo`), filesystem roots, and host process state. New sessions default to the sandboxed `agent` mode.

## Select the host and session first

1. If `host_list` is available, treat the connection as a federated gateway. If the user explicitly names a host, use that exact `host_id`; otherwise call `host_list` before choosing among multiple machines.
2. If the user explicitly names a session ID, preserve that exact ID. Use `session_list(host_id=...)` when a host is selected.
3. Match the target project to an existing session `cwd`. If one clearly matches on the selected host, use it. The same `session_id` may exist on different hosts.
4. If no session matches and `session_start` is available, start one with explicit `host_id` when supported and a logical named-root path such as `src/project`. Do not invent an absolute host path, do not pass a yolo option, and do not retry an unknown root by weakening path constraints.
5. Call `session_info` with explicit `host_id` when supported after selecting or creating the session, before delegating work. Continue passing that `host_id` with session-scoped calls.
6. Do not silently switch to a different host or session midway through a task.

On a federated gateway, prefer `host_list` → `session_list(host_id=...)` → `session_start(host_id=...)` when needed → `session_info(host_id=..., session_id=...)` → task tools. An unqualified `session_id` is a compatibility path only; if ownership is ambiguous, do not guess. `session_stop` and `session_restart` may act only on sessions owned by the selected host's public supervisor; never use them to try to control a separately started CLI/yolo session.

On a direct single-host Temote endpoint without `host_list`, keep using the existing session-only workflow.

Do not ask the user to repeat a session ID or logical path that Temote MCP can discover or that the current task already supplies.

## Delegate machine work to the local agent

Every backend follows the same contract: `*_status` probes the installed backend once, `*_task_start` accepts an idempotent task (mandatory fresh UUID `operation_id`), `*_task_get` reads/reconciles a retained task (`after_revision` honored after reconciliation), and `*_task_control` applies typed `steer`/`resume`/`interrupt` actions. Detailed transcripts and output are exposed only as bounded scoped evidence — read them with `evidence_read`, never expect raw child output inline.

- `codex_*` runs the local Codex app-server over stdio.
- `opencode_*` runs a per-task `opencode serve` on loopback.
- `devin_*` runs a per-task `devin acp` child over stdio; `cloud: true` relays through `devin acp --cloud` (omit `model`/`agent` in cloud mode).
- `devin_cloud_*` uses the Devin Cloud API directly.

`task_list` is the bounded read-only projection of the session's delegated tasks across all backends — use it to find a task's `backend` + `task_id` reference before `*_task_get` or `*_task_control`. A backend marked `unconfirmed` could not be read and is never faked as empty; its tasks are absent, not closed.

Call the matching `*_status` first and treat its version/model/effort result as diagnostic compatibility metadata, not as a version allowlist or proof that a task will succeed. Start and control require a fresh UUID `operation_id`; preserve it for an exact retry and never retry an uncertain side effect with a new ID. Pre-thread startup failures may be retried with the same start ID; once a thread/turn request may have been sent, keep the task in reconciliation and do not blindly replay it.

Use `*_task_get` to reconcile remote truth, `reconciliation_required`, `unknown`, approval waits, and process restarts before deciding whether to control a task. If it returns `reconciliation_deferred: true`, another Temote process owns the live runtime: use the persisted status and revision, and do not issue control through the secondary process. A session stop/restart finalizes nonterminal tasks owned by the ended full session instance as `interrupted`; while another process holds the runtime lease, its owner observes session termination, shuts down the child, and finalizes the task. Retained records remain fenced from a replacement instance. Unexpired terminal records are not evicted to make capacity; when no expired terminal record is available, a new start is rejected.

Temote yolo does not authorize delegated child mutations: command/file-change approval requests still use the explicit user-approval path and fail closed if it is unavailable. Control is limited to typed actions; do not attempt to tunnel arbitrary backend JSON-RPC or a remote shell through these tools.

## Jobs

A task or run that exceeds the foreground timeout returns a session-owned `job_id`. Poll it with `poll_job` until it finishes when completion is needed for the user's current task; use `job_list` for the bounded snapshot of session jobs and `stop_job` when running work is no longer needed or must be cancelled. Jobs are session-owned and cancelled when the session stops.

Do not tell the user that work is complete while a required job is still running. Do not ask the user to wait instead of polling a job that can be completed in the current turn.

## Approval model

`ask` uses Temote MCP's local approval boundary for host/network-sensitive structured operations. The default sandboxed `agent` mode skips only that Temote-local prompt for otherwise-valid structured operations; it never widens sandbox, path, network, or tool-specific capability. Yolo sessions intentionally skip Temote MCP approval prompts and path/sandbox restrictions.

Do not add a redundant conversational confirmation for an operation the user already explicitly requested merely because Temote MCP may also display its own host approval UI. Still follow any confirmation or authorization rules imposed by the current agent/client.

Never infer that yolo mode disables authorization outside Temote MCP.

## Supervisor upgrades

`temote-mcp up` owns HTTP/ingress, not the session supervisor. When the user explicitly asks to apply an already-installed Temote binary to the running supervisor and local command execution is available, use `temote-mcp upgrade --dry-run` first and inspect the complete plan before `temote-mcp upgrade`. Do not substitute re-running `temote-mcp up` for a supervisor handoff.

The dry-run is the source of truth for the transition. Check supervisor compatibility, `blocked_sessions`, in-flight-operation blockers, and direct-ingress actions. It may also report an ingress blocker when the current ingress cannot be reconstructed safely, for example because required restart context was interactive-only. Do not proceed with the destructive upgrade while any blocker remains.

A successful upgrade performs a same-PID supervisor handoff, restores and probes the intended active-session set, and reports deterministic partial-state details if restore fails. When direct ingress is active, it is left untouched only when already healthy on the target binary; otherwise Temote restarts it from its durable non-secret recipe and requires `/healthz` to recover before reporting success. After the supervisor/session transition succeeds, Temote also reconciles the binary-owned Codex plugin transactionally; report any exact manual follow-up or client-restart requirement returned by the command rather than assuming a running client has reloaded the plugin.

Do not persist or reconstruct plaintext credentials to force a transition. A supervisor that predates the handoff protocol requires one manual supervisor restart before later compatible releases can use `upgrade`.

## Failure handling

When a tool fails:

- preserve the exact meaningful error and identify whether it is a session, permission-root, backend availability, task reconciliation, or approval failure;
- inspect current session and task state before retrying if the failure could be caused by a stopped/replaced session or an uncertain side effect;
- do not weaken sandbox or yolo settings as a troubleshooting shortcut;
- do not retry non-idempotent operations blindly — reuse the same `operation_id` only for exact retries;
- prefer a generic repository/product fix over a one-off workaround when implementing software changes.

## Completion

For delegated tasks, drive the task to a terminal state in the same turn when tools permit it: start, poll `*_task_get` or `job_id`, reconcile, then report the concrete result, evidence references, and any remaining limitation.

## Direct-HTTP remote upgrade

Use `upgrade_preflight` before proposing an upgrade. `upgrade_apply` accepts an
active managed normal `session_id` and optional `expected_version`; never invent
or pass an executable path, URL, command, argv, or environment. Both ask and
agent sessions require explicit approval by the local user. The accepted HTTP
connection is intentionally closed after its response is flushed. Reconnect to
the same endpoint with normal authentication, verify process identity from
initialize or ping, and call `upgrade_status(transaction_id)` until terminal.
Temote cannot force the MCP client to reconnect. These tools are available only
on authenticated direct HTTP, not stdio or the gateway.
