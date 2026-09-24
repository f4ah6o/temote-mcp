# Managed sessions and named roots

Temote has one session-lifecycle owner: `temote-mcp supervisor`. It owns every `RuntimeHandle`, the durable lifecycle state, and the reconnectable local approval broker used by `ask` and child-approval flows.

`temote-mcp serve` / `temote-mcp up` are authenticated HTTP/ingress processes only. They connect to the existing local supervisor through the same-user `0600` Unix control socket for public `session_start` / `session_stop`. Tailscale local-OAuth approvals are proxied through that socket to `temote-mcp session console`; the public HTTP endpoint never exposes approval attachment.

tmux, Herdr, systemd, or another process keeper may keep the lifecycle supervisor visible or restart it, but they are not the session-level source of truth.

## Named-root configuration

`TEMOTE_MCP_ROOTS` separates the logical namespace from host filesystem paths.

```sh
TEMOTE_MCP_ROOTS='src=~/src'
```

For multiple roots, prefer JSON:

```sh
TEMOTE_MCP_ROOTS='{"src":"~/src","work":"~/work"}'
```

Root names accept only ASCII letters, digits, `-`, and `_`. The configured root is canonicalized first. This allows an administrator-selected alias such as `~/src -> /Volumes/devstorage/Developer`, while descendant symlinks or `..` traversal that escape the canonical physical root are rejected. Missing root configuration fails closed; there is no HOME, `/`, cwd, or repository fallback.

## Local session supervisor

Run one foreground supervisor:

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor
```

Manage sessions from another terminal:

```sh
temote-mcp session start mitsumori --path src/mitsumori-core
temote-mcp session list
temote-mcp session info mitsumori
temote-mcp session permission mitsumori status
temote-mcp session permission mitsumori allow /path/to/extra-root
temote-mcp session permission mitsumori revoke /path/to/extra-root
temote-mcp session permission mitsumori ask
temote-mcp session permission mitsumori agent
temote-mcp session permission mitsumori yolo
temote-mcp session restart-policy mitsumori on-failure
temote-mcp session stop mitsumori
temote-mcp session restart mitsumori
```

`session list` reports `starting`, `active`, `stopping`, `stopped`, or `crashed` and probes the runtime socket before treating a session as live. Stale metadata with a dead socket is never shown as `active`.

`session info` includes the non-secret `host_id`, cwd, permitted directories, permission mode, start/stop timestamps, exit reason, last error, logical named-root path when available, restart policy, restart count, most recent restart time, pending restart time, and any terminal restart-limit reason.

For compatibility, `temote-mcp start <id>` remains available. It asks the running local supervisor to start the current directory instead of owning the runtime itself. `--yolo` remains a local-only option. The public MCP `session_start` contract still cannot request yolo mode.

Detached permission management is local-only and travels over the same owner-only supervisor Unix socket. `permission allow/revoke` keeps the existing canonical-path and symlink containment rules; the session cwd cannot be revoked. `permission ask/agent/yolo` is explicit, and none of these mutations restart the runtime or discard runtime state. Persisted permitted roots are restored when that same session/cwd is explicitly restarted.

## Approval console attachment

Attach approval input separately:

```sh
temote-mcp session console
```

The approval console is not the runtime owner. stdin EOF, Ctrl-C, PTY disconnect, or terminal close detaches the console without stopping session runtimes. While no console is attached, approval-required operations fail closed. A later `session console` can reconnect and service subsequent approval requests.

HTTP `serve/up` has no separate approval console and owns no session runtimes. `serve/up` verifies the local control-protocol version at startup and fails closed if the lifecycle supervisor must be upgraded/restarted first. Tailscale OAuth approval and runtime host approvals use the same reconnectable `temote-mcp session console`. If the HTTP origin or ingress restarts, session runtimes remain owned by the lifecycle supervisor.

## Local activity viewer

Use the local CLI to observe recent and live activity owned by the running supervisor:

```sh
temote-mcp activity
temote-mcp activity my-project --tail 100
temote-mcp activity my-project --tail 0 --no-follow
```

The optional session ID is an exact filter. `--tail` is applied after filtering, accepts `0` through `1024`, and defaults to `100`; replay is shown oldest first. The command follows new activity by default. `--no-follow` prints the bounded replay and exits after its end marker. The viewer does not start or reconnect a supervisor, and a valid session ID with no retained events produces an empty replay rather than an error.

Each event is one line in local time and contains the session routing ID, shortened instance and operation IDs, operation, state, and a fixed safe summary when one applies. Covered operations include session lifecycle and permissions, file and Git tools, commands and jobs, delegated developer tools, supported integrations, accepted Codex task calls, and supervisor upgrade phases. A completed Codex start or control event means that Temote accepted the call; it does not mean the Codex turn finished.

Activity is a best-effort, process-memory diagnostic stream. The supervisor retains at most 4096 events and 8 MiB; one serialized event is at most 2048 bytes and its summary at most 512 UTF-8 bytes. Live broadcast capacity is 1024 events, each producer queue holds 256 typed updates, and at most 16 viewers may attach. Queue saturation, process termination, ingress failure, or broker contention can drop an update before it receives a sequence number, so no later gap can count that loss. A history-truncated notice means older retained events were evicted. A live gap reports a global sequence interval and must not be read as the exact number of matching events omitted by a session filter. An intentional tail limit and a quiet session are not gap reports.

The viewer never includes commands, paths, Git branch or arbitrary remote names, URLs, prompts, payloads, stdout/stderr, environment values, credentials, or raw errors. Session routing IDs and shortened generated identifiers are intentionally visible. Activity is not an audit log or a source of current operation truth, and no activity history is written to disk.

In follow mode, Ctrl-C, TTY EOF, or supervisor socket EOF detaches only the viewer and exits successfully; EOF from a pipe or `/dev/null` on stdin is ignored. A closed output pipe also exits successfully. Invalid frames and output failures exit unsuccessfully. In `--no-follow` mode stdin is ignored, a complete end marker exits successfully, and socket EOF before that marker exits unsuccessfully. A supervisor restart or successful same-PID binary handoff resets the activity generation and in-memory history and closes existing viewers. Run the command again to attach to the replacement supervisor.

This attachment exists only on the owner-only local Unix control socket. It is not exposed through stdio MCP, authenticated public HTTP, or the multi-host gateway, and attaching or disconnecting does not change a session, approval, job, or runtime.

## Runtime and failure isolation

The session Unix socket remains the runtime boundary for MCP operations and host bridges. CLI and HTTP-managed sessions use the same runtime implementation for sandbox permissions, approval state, 1Password bridge state, kintone bridges, metadata, and socket lifecycle.

Per-connection failures are isolated from the runtime. Broken pipes, connection resets, malformed messages, oversized messages, read timeouts, client disconnects, and response write failures terminate only that connection. In particular, probe and yolo-approval response writes do not propagate through the runtime loop.

Listener failure, runtime task panic/join failure, or another unexpected core-runtime termination is runtime-fatal for that session. The monitor records `crashed`, `stopped_at`, an exit reason, and the last error. One session failure does not stop other sessions owned by the same supervisor.

## Persistent lifecycle state

Each session keeps normal session metadata plus a private lifecycle state file. Lifecycle transitions are:

```text
starting -> active -> stopping -> stopped
                    \-> crashed
```

A graceful explicit stop becomes `stopped`. Unexpected termination becomes `crashed`. On local supervisor startup, stale sockets are removed and metadata that claimed a live runtime but has no live socket is reconciled to `crashed`.

## Supervisor binary handoff

After installing a replacement Temote binary, run `temote-mcp upgrade --dry-run` and then `temote-mcp upgrade`. The target executable must report the same local control protocol, lifecycle schema, and restore-plan schema as the running supervisor; incompatible generations are rejected before any active runtime is stopped.

If `blocked_session_count` is nonzero, inspect affected sessions with `session info <id>` for missing workspaces or restart context. Stop sessions that no longer need restoration with `session stop <id>` before retrying. On Linux, `helper_generation` reports sandbox helper compatibility; if an older supervisor does not report it, the upgrade CLI inspects the helper bundled with the installed binary.

The implementation uses coordinated restart/restore, not live in-process task transfer. The old supervisor fences lifecycle mutations, validates named-root/cwd resolution and memory-only restart context, and quiesces all active runtimes. Quiesce fails closed when an integration call or approval is in flight. The owner-only restore plan contains session identity, path/permission/restart metadata, and restart-context key names only—never credential values. After graceful drain, the process `exec`s `temote-mcp supervisor --restore-plan ...` with the same PID. The replacement recreates only the planned active sessions and verifies metadata plus every session-socket probe before deleting the plan.

Direct `serve/up` ingress remains a separate process; it can keep running when the control protocol is compatible. Protocol/lifecycle-schema-changing handoffs are deliberately rejected rather than guessing an ingress restart. If `exec` itself fails, the old supervisor attempts to recreate drained sessions from its in-memory restart specifications and clears the fence. If replacement startup/restore fails after `exec`, the non-secret restore plan remains for diagnosis/recovery. After supervisor and session health checks pass, `upgrade` transactionally refreshes the binary-owned Codex plugin and reports the required restart of an already-running Codex client. A pre-handoff-protocol supervisor needs one manual restart to bootstrap this path.

Restart policy defaults to `never`. `temote-mcp session restart-policy <id> on-failure` enables automatic restart only after unexpected runtime failure; graceful stop never restarts. Automatic restart uses bounded exponential delays of 1, 2, 4, 8, and 16 seconds and then settles in `crashed` after five attempts. Lifecycle state records `restart_count`, `last_restart_at`, `next_restart_at`, and `restart_limit_reason`. The original captured start environment is retained only in supervisor memory and is never persisted; after the supervisor process itself restarts, pending credential-bearing automatic restart is intentionally not resumed and the session remains `crashed` with an explanatory reason until an explicit `session restart`.

## HTTP managed sessions

An authenticated direct HTTP MCP client can use:

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
session_stop(session_id="my-project")
```

HTTP managed sessions are always `yolo=false` and default to `agent`, so the normal structured development workflow runs without a local approval console while sandbox/path/network boundaries stay in force. `ask` remains available as an explicit stricter local transition. Existing approval-gated host operations keep their tool-specific validation and capability rules in every mode. The lifecycle supervisor marks HTTP-created runtimes in memory; public `session_stop` accepts only that set and cannot stop local CLI/yolo sessions. HTTP ownership is intentionally not a permission persisted into session metadata.

`session_list` and `session_info` expose durable stopped/crashed state as well as active sessions. Other session-bound MCP tools still require a live runtime socket.

`session_start` and `session_stop` are exposed only by the authenticated direct HTTP `serve` endpoint. Direct `temote-mcp up` is single-host per public endpoint: one endpoint maps to one local lifecycle supervisor and host-local session store. Reusing one Cloudflare Tunnel token/hostname concurrently across multiple direct-ingress hosts is unsupported because Cloudflare replica routing is not session-aware. Set `TEMOTE_MCP_HOST_ID` for a stable non-secret diagnostic identity (OS hostname is the fallback). For a single public endpoint routing to multiple Temote hosts, use `temote-mcp gateway-agent` with the Worker/Durable Objects gateway. The gateway generation/lease routing contract is unchanged. The existing public exclusion of `without_sandbox` remains unchanged.

## Optional terminal integration

Herdr or tmux may be used to keep the single `temote-mcp supervisor` terminal organized or visible. They are optional UI/process-retention layers only; Temote remains responsible for session ownership, lifecycle metadata, crash detection, and restart commands.
