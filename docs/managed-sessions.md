# Managed sessions and named roots

Temote has one session-lifecycle owner: `temote-mcp supervisor`. It owns every `RuntimeHandle`, the durable lifecycle state, and the reconnectable local approval broker.

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
temote-mcp session permission mitsumori yolo
temote-mcp session restart-policy mitsumori on-failure
temote-mcp session stop mitsumori
temote-mcp session restart mitsumori
```

`session list` reports `starting`, `active`, `stopping`, `stopped`, or `crashed` and probes the runtime socket before treating a session as live. Stale metadata with a dead socket is never shown as `active`.

`session info` includes the non-secret `host_id`, cwd, permitted directories, permission mode, start/stop timestamps, exit reason, last error, logical named-root path when available, restart policy, restart count, most recent restart time, pending restart time, and any terminal restart-limit reason.

For compatibility, `temote-mcp start <id>` remains available. It asks the running local supervisor to start the current directory instead of owning the runtime itself. `--yolo` remains a local-only option. The public MCP `session_start` contract still cannot request yolo mode.

Detached permission management is local-only and travels over the same owner-only supervisor Unix socket. `permission allow/revoke` keeps the existing canonical-path and symlink containment rules; the session cwd cannot be revoked. `permission ask/yolo` is explicit, and none of these mutations restart the runtime or discard runtime state. Persisted permitted roots are restored when that same session/cwd is explicitly restarted.

## Approval console attachment

Attach approval input separately:

```sh
temote-mcp session console
```

The approval console is not the runtime owner. stdin EOF, Ctrl-C, PTY disconnect, or terminal close detaches the console without stopping session runtimes. While no console is attached, approval-required operations fail closed. A later `session console` can reconnect and service subsequent approval requests.

HTTP `serve/up` has no separate approval console and owns no session runtimes. `serve/up` verifies the local control-protocol version at startup and fails closed if the lifecycle supervisor must be upgraded/restarted first. Tailscale OAuth approval and runtime host approvals use the same reconnectable `temote-mcp session console`. If the HTTP origin or ingress restarts, session runtimes remain owned by the lifecycle supervisor.

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

Restart policy defaults to `never`. `temote-mcp session restart-policy <id> on-failure` enables automatic restart only after unexpected runtime failure; graceful stop never restarts. Automatic restart uses bounded exponential delays of 1, 2, 4, 8, and 16 seconds and then settles in `crashed` after five attempts. Lifecycle state records `restart_count`, `last_restart_at`, `next_restart_at`, and `restart_limit_reason`. The original captured start environment is retained only in supervisor memory and is never persisted; after the supervisor process itself restarts, pending credential-bearing automatic restart is intentionally not resumed and the session remains `crashed` with an explanatory reason until an explicit `session restart`.

## HTTP managed sessions

An authenticated direct HTTP MCP client can use:

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
session_stop(session_id="my-project")
```

HTTP managed sessions are always `yolo=false`. Existing approval-gated host operations remain approval-gated. The lifecycle supervisor marks HTTP-created runtimes in memory; public `session_stop` accepts only that set and cannot stop local CLI/yolo sessions. HTTP ownership is intentionally not a permission persisted into session metadata.

`session_list` and `session_info` expose durable stopped/crashed state as well as active sessions. Other session-bound MCP tools still require a live runtime socket.

`session_start` and `session_stop` are exposed only by the authenticated direct HTTP `serve` endpoint. Direct `temote-mcp up` is single-host per public endpoint: one endpoint maps to one local lifecycle supervisor and host-local session store. Reusing one Cloudflare Tunnel token/hostname concurrently across multiple direct-ingress hosts is unsupported because Cloudflare replica routing is not session-aware. Set `TEMOTE_MCP_HOST_ID` for a stable non-secret diagnostic identity (OS hostname is the fallback). For a single public endpoint routing to multiple Temote hosts, use `temote-mcp gateway-agent` with the Worker/Durable Objects gateway. The gateway generation/lease routing contract is unchanged. The existing public exclusion of `without_sandbox` remains unchanged.

## Optional terminal integration

Herdr or tmux may be used to keep the single `temote-mcp supervisor` terminal organized or visible. They are optional UI/process-retention layers only; Temote remains responsible for session ownership, lifecycle metadata, crash detection, and restart commands.
