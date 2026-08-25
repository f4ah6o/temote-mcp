# Temote MCP session lifecycle supervision hardening

Date: 2026-08-25

## Background

`temote-mcp start <session-id>` currently owns both the session runtime and the terminal approval UI in one process lifetime. A terminal/PTY/stdin lifecycle event therefore determines the runtime lifetime. Managed sessions under `SessionSupervisor` already reuse `spawn_runtime()`, but lifecycle state is not durable and finished handles are discarded without preserving a crash reason.

The target is to make Temote itself the source of truth for session runtime ownership. Herdr, tmux, systemd, or another terminal/process keeper may keep the single supervisor process visible, but must not become the session-level owner.

## Current failure analysis

| Failure mode | Current behavior | Classification | Required behavior |
| --- | --- | --- | --- |
| stdin EOF in `run_cli_console` | console returns, caller immediately `handle.shutdown()` | console-local | detach console; runtime survives |
| stdin EOF in shared supervisor console | console task returns and receiver is dropped | console-local | runtimes survive; approvals fail closed; console can be attached again |
| Ctrl-C in single-session CLI | console returns then runtime is shut down | graceful supervisor/session stop | supervisor-owned graceful stop only when explicitly requested |
| approval console task exits | sender eventually becomes disconnected | console-local | runtime survives; approval-required operations fail closed |
| UnixListener `accept()` error | `?` exits `run_runtime()` | runtime-fatal | record crash reason and preserve lifecycle metadata |
| client disconnect during message read | isolated in `receive_session_message()` | connection-local | keep isolated |
| malformed IPC | isolated in `receive_session_message()` | connection-local | keep isolated |
| oversized IPC | isolated in `receive_session_message()` | connection-local | keep isolated |
| IPC read timeout | isolated in `receive_session_message()` | connection-local | keep isolated |
| Probe response write failure | `stream.write_all(...).await?` exits runtime | connection-local bug | ignore/log connection failure; runtime survives |
| YOLO approval response write failure | `stream.write_all(...).await?` exits runtime | connection-local bug | ignore/log connection failure; runtime survives |
| bridge response write failure | already best-effort in spawned task | connection-local | keep isolated |
| approval response write failure | already best-effort in spawned task | connection-local | keep isolated |
| child IPC task panic | detached task panic is not propagated | connection-local unless core invariant task | runtime survives; do not promote connection handler panics |
| runtime `JoinHandle<Result<()>>` returns `Err` | supervisor drops finished handles via `retain` and reason is lost | runtime-fatal | persist `crashed`, exit reason, last error |
| runtime task panic / join error | reason is lost unless awaited by shutdown path | runtime-fatal | persist `crashed` with join error |
| metadata save failure during runtime | permission mutations report error; final save only logs | recoverable/runtime-observability failure | do not crash a healthy runtime solely for final observability failure where avoidable; surface last error when possible |
| socket cleanup failure | ignored after runtime exits | recoverable | runtime remains stopped/crashed; preserve metadata and cleanup/reconcile on next supervisor start |
| stale metadata/socket after supervisor restart | stale socket is removed at new runtime start; stale metadata may disappear from `session_list` | recoverable | reconcile metadata/socket; never show dead socket as `active` |
| supervisor process termination | all in-process runtimes terminate with it | runtime-fatal / ownership boundary | on next supervisor start mark previously live sessions `crashed`; manual restart is available |
| one session runtime failure | finished handle is removed opportunistically | runtime-fatal for that session | record only that session crash; other sessions remain active |

## Design

### Ownership

```text
temote-mcp supervisor
└─ SessionSupervisor
   ├─ SessionRuntime: mitsumori
   ├─ SessionRuntime: role-policy
   ├─ SessionRuntime: n8n
   └─ ApprovalConsole (attachable / optional)
```

`SessionSupervisor` remains the owner of `RuntimeHandle`. `spawn_runtime()` and `run_runtime()` remain the reusable runtime implementation.

A local supervisor control socket is added for CLI lifecycle commands. The supervisor is one foreground Temote process. A terminal keeper may keep this one process open, but session runtime ownership remains inside Temote.

### CLI

Target surface:

```text
temote-mcp supervisor
temote-mcp session start <id> --path <named-root-relative-path>
temote-mcp session list
temote-mcp session info <id>
temote-mcp session stop <id>
temote-mcp session restart <id>
temote-mcp session console
```

`session console` is an attachable approval console. Disconnecting it denies pending prompts and causes new approval-required operations to fail closed until another console attaches.

The legacy `temote-mcp start <id>` command remains as a compatibility path. Internally it must use the supervisor/runtime abstraction rather than a separate runtime implementation. If it cannot safely preserve the old current-directory semantics without weakening named-root/sandbox boundaries, it may require/resolve a configured named-root path and must fail closed rather than bypass the supervisor.

### Lifecycle metadata

Persist at least:

- `status`: `starting | active | stopping | stopped | crashed`
- `started_at`
- `stopped_at`
- `process_id` (supervisor PID for in-process runtimes)
- `exit_reason`
- `last_error`
- `cwd`
- `permission_mode` (derived from existing `yolo` field or serialized explicitly)
- restartable logical path when the session was started from a named root

A successful explicit shutdown transitions `active -> stopping -> stopped`. Unexpected `run_runtime()` error or runtime task join failure transitions to `crashed`.

`session list` must use a socket probe in addition to metadata. A metadata record that claims `active` but has no live socket must not be displayed as active.

### Restart policy

Phase 1 implements crash detection, durable crash reason, and manual restart. Automatic `on-failure` restart is intentionally deferred to a separate issue to avoid introducing an unbounded crash loop in the same change. Default restart policy is therefore `never`.

A follow-up issue should define `on-failure` with backoff/rate limiting.

### Approval console

Approval routing is independent from runtime lifetime:

- no console attached: fail closed immediately
- console disconnect: deny in-flight prompt(s), keep runtimes alive
- console reconnect: subsequent prompts may be reviewed normally
- YOLO behavior remains an explicit local session permission mode and is not made remotely creatable through public MCP

### Failure isolation

`run_runtime()` must not propagate per-connection I/O failures. In particular Probe and YOLO approval writes must not use `?` in the runtime loop.

Listener failure remains runtime-fatal. Runtime command-channel closure without an explicit shutdown is also treated as unexpected termination and recorded as a crash.

## Acceptance criteria

- [ ] terminal/stdin disconnect is not a session runtime stop reason
- [ ] approval console disconnect leaves all runtimes alive
- [ ] approval-required operations fail closed while no console is attached
- [ ] approval console can reconnect
- [ ] malformed, oversized, timed-out, disconnected, and response-write-failing IPC clients do not stop the runtime
- [ ] runtime crash is persisted as `crashed` with exit reason / last error
- [ ] graceful stop is persisted as `stopped`
- [ ] stale metadata/socket state is reconciled after supervisor restart
- [ ] multiple sessions are owned by one supervisor
- [ ] failure of one session does not stop another
- [ ] `session list` never reports a dead socket as `active`
- [ ] existing sandbox, approval, 1Password, kintone MCP, and cli-kintone security boundaries remain unchanged
- [ ] existing `temote-mcp start` remains compatible where safely possible and shares the same runtime abstraction
- [ ] README / managed session / usage documentation is updated in English and Japanese
- [ ] tests pass

## Required tests

1. stdin EOF / console disconnect does not stop runtime
2. client disconnect before response does not stop runtime
3. malformed IPC does not stop runtime
4. oversized IPC does not stop runtime
5. write failure does not stop runtime
6. runtime crash is recorded as `crashed`
7. graceful stop becomes `stopped`
8. supervisor restart reconciles stale metadata/socket
9. multiple sessions are managed by one supervisor
10. one session failure does not affect another

## Deferred follow-up

Automatic restart policy (`on-failure`) with bounded exponential backoff or a restart-rate window is deferred unless implementation remains small after lifecycle persistence and local supervisor control are complete.
