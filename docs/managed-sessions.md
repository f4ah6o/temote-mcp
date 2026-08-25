# Managed sessions and named roots

Temote has two supervisor entry points that share the same `SessionSupervisor` / session runtime implementation:

- `temote-mcp supervisor`: local session-lifecycle supervisor controlled by `temote-mcp session ...`
- `temote-mcp serve` / `temote-mcp up`: authenticated HTTP supervisor used by MCP `session_start` / `session_stop`

Both own `RuntimeHandle`s directly. tmux, Herdr, systemd, or another terminal/process keeper may keep a Temote supervisor process visible, but they are not the session-level source of truth.

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
temote-mcp session stop mitsumori
temote-mcp session restart mitsumori
```

`session list` reports `starting`, `active`, `stopping`, `stopped`, or `crashed` and probes the runtime socket before treating a session as live. Stale metadata with a dead socket is never shown as `active`.

`session info` includes the cwd, permitted directories, permission mode, start/stop timestamps, exit reason, last error, logical named-root path when available, and restart policy.

For compatibility, `temote-mcp start <id>` remains available. It asks the running local supervisor to start the current directory instead of owning the runtime itself. `--yolo` remains a local-only option. The public MCP `session_start` contract still cannot request yolo mode.

## Approval console attachment

Attach approval input separately:

```sh
temote-mcp session console
```

The approval console is not the runtime owner. stdin EOF, Ctrl-C, PTY disconnect, or terminal close detaches the console without stopping session runtimes. While no console is attached, approval-required operations fail closed. A later `session console` can reconnect and service subsequent approval requests.

The HTTP `serve` supervisor retains its existing local approval console behavior. If that console disappears, approval delivery fails closed; managed runtimes remain owned by `serve` until the supervisor itself terminates.

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

The first implementation intentionally uses restart policy `never`. `temote-mcp session restart <id>` provides manual restart for stopped, crashed, or currently active local-supervisor sessions. Automatic `on-failure` restart with bounded backoff/rate limiting is tracked separately.

## HTTP managed sessions

An authenticated direct HTTP MCP client can use:

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
session_stop(session_id="my-project")
```

HTTP managed sessions are always `yolo=false`. Existing approval-gated host operations remain approval-gated. `session_stop` stops only runtimes owned by the current HTTP `SessionSupervisor`.

`session_list` and `session_info` expose durable stopped/crashed state as well as active sessions. Other session-bound MCP tools still require a live runtime socket.

`session_start` and `session_stop` are exposed only by the authenticated direct HTTP `serve` endpoint. The Cloudflare Workers + Durable Objects multi-host gateway does not advertise them and gains no host-selection contract from this lifecycle change. The existing public exclusion of `without_sandbox` remains unchanged.

## Optional terminal integration

Herdr or tmux may be used to keep the single `temote-mcp supervisor` terminal organized or visible. They are optional UI/process-retention layers only; Temote remains responsible for session ownership, lifecycle metadata, crash detection, and restart commands.
