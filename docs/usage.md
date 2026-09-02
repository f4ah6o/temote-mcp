# Using Temote MCP

[日本語](usage.ja.md)

## Sessions

For local work, start one Temote lifecycle supervisor and then create named-root sessions from another terminal:

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor

temote-mcp session start my-project --path src/my-project
temote-mcp session list
temote-mcp session info my-project
```

Use `temote-mcp session console` when local approval input is required. Closing that console or sending stdin EOF detaches it without stopping the runtime. While no console is attached, approval-required operations fail closed.

After replacing the installed binary, run `temote-mcp upgrade --dry-run` and then `temote-mcp upgrade` for a compatible same-PID supervisor handoff with coordinated session restart/restore. No credential values are persisted; missing restart context or an in-flight operation aborts the transition, and every planned session is verified before success. A supervisor from before the handoff protocol needs one manual restart first.

`session list` includes durable `starting`, `active`, `stopping`, `stopped`, and `crashed` states. `session info` includes the working directory, permitted roots, permission mode, timestamps, exit reason, and last error. A dead or ambiguous socket is never silently treated as active. Manual restart is available with `temote-mcp session restart <id>`; automatic restart is not enabled.

For compatibility, `cd ~/src/my-project && temote-mcp start my-project` asks the running local supervisor to start the current directory. `temote-mcp start my-project --yolo` remains the deliberately unrestricted local-only form.

Relative paths resolve from the session working directory.

### Managed sessions from HTTP

Set `TEMOTE_MCP_ROOTS` on the lifecycle supervisor, keep that supervisor running, and start `temote-mcp up` separately:

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor
# another terminal/service
temote-mcp up
```

For multiple roots, use a JSON object on the supervisor process instead of a separator-based list:

```sh
export TEMOTE_MCP_ROOTS='{"src":"~/src","work":"~/work"}'
temote-mcp supervisor
```

The client calls `session_list`, then `session_start(path="src/project")` when needed, then `session_info`. The configured root itself is canonicalized, so a host alias such as `~/src -> /Volumes/devstorage/Developer` is allowed. Descendant symlinks or `..` traversal that resolve outside that canonical physical root are rejected. Missing roots fail closed with no HOME, `/`, cwd, or repository fallback.

`session_stop` can stop only sessions marked as HTTP-owned by the lifecycle supervisor; it cannot stop local CLI/yolo sessions even though they share the same supervisor process. HTTP managed sessions are always non-yolo and retain the same local approval gates through `temote-mcp session console`. Stopped/crashed metadata remains visible through `session_list` / `session_info`; ordinary session-bound tools still require an active socket. `temote-mcp down` stops only the HTTP origin and its managed ingress child, not the lifecycle supervisor or its sessions. In a repository checkout, `just up/down` are development wrappers around these installed-binary commands.

## Migrating an older always-on runtime

Older repository checkouts used `just up` to launch `temote-mcp serve` and `cloudflared` as sibling processes and recorded both PIDs in `~/.cache/temote-mcp/up.pids`. Current installed deployments use `temote-mcp up`, one locked `up.pid`, and child-process ownership. Replacing the executable does not replace an already-running process.

After installing a current binary, migrate the legacy runtime state once:

```sh
cargo binstall temote-mcp --force
temote-mcp migrate --dry-run
temote-mcp migrate
TEMOTE_MCP_ROOTS='src=~/src' temote-mcp supervisor
# another terminal/service
temote-mcp up --profile cloudflare
```

Migration validates the legacy state file and verifies live process names before signaling anything. It fails closed if a PID belongs to an unexpected process. It removes stale legacy state or stops only the validated legacy `temote-mcp serve` + `cloudflared` pair. `public.env`, `tunnel-token`, session metadata, sockets, and independently started `temote-mcp start <session>` processes are not changed. Re-running `temote-mcp migrate` when no legacy state remains is a no-op.

## Permission roots

A normal session starts with its canonical startup directory as its permitted root. Local named-root selection determines which project directory is used; remote `session_start` can only resolve paths below administrator-configured named roots. Normal sessions reject paths, symlink targets, and command working directories that escape their permitted roots.

The legacy inline `/permission ...` terminal command UI is not the owner of detached runtimes and is not exposed through the first supervisor control surface. This does not widen permissions: the runtime remains fail-closed with its persisted permitted roots.

## Commands

`execute` runs argv without a shell. In normal sessions it runs inside Temote MCP's sandbox with network disabled. If the command completes within the foreground timeout, the result is returned immediately; otherwise it returns a `job_id`.

Use `start_command` when work should be backgrounded immediately, then `poll_job` until completion or `stop_job` to cancel it. Jobs belong to their session, have a two-hour lifetime limit, and are cancelled when the session stops. A session can have up to eight active sandbox jobs.

The combined stdout/stderr retained for a command is capped at 1 MiB and reports when output was truncated.

## Files and images

- `list_directory` lists a directory.
- `read_file` reads UTF-8 text.
- `get_image` returns supported image content through MCP.
- `write_file` writes UTF-8 text inside the selected permission mode.

## Git

Ordinary sandboxed commands keep Git metadata read-only. Use the dedicated tools instead:

- `git_add` stages explicit paths.
- `git_commit` commits the current index with hooks and signing disabled.
- `git_fetch` fetches a configured remote.
- `git_pull` is fast-forward-only.
- `git_push` pushes the current branch and exposes no force option or arbitrary remote URL/refspec.

Remote Git operations are host operations and require local approval in normal sessions.

## Yolo mode

```sh
temote-mcp start my-project --yolo
```

Yolo mode intentionally removes Temote MCP's path restrictions, command sandbox, and local approval prompts. Commands run with the filesystem, environment, process, and network permissions of the user running Temote MCP. This does not disable authorization or confirmation imposed by an MCP client or another external system.

The detached supervisor does not automatically promote a running normal session to yolo mode. Start yolo explicitly through the local-only compatibility command when that trust level is intended.

## Local stdio

For MCP clients that launch Temote MCP directly:

```sh
temote-mcp mcp
```

Local stdio can expose the explicitly approval-gated `without_sandbox` tool. The public HTTP endpoint does not expose it.

## Safety notes

- Do not permit broad roots such as an entire home directory when a narrower project path is sufficient.
- There is no secret-file denylist; permitted roots are the primary filesystem boundary.
- Runtime audit records operation/status/timing metadata, not command arguments, command output, authenticated identity fields, or secret values.
- Secret-bearing integrations keep credentials in the session process rather than session metadata.
