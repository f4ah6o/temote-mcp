# Using Temote MCP

[日本語](usage.ja.md)

## Sessions

Start Temote MCP from the directory an agent should be able to work in:

```sh
cd ~/src/my-project
temote-mcp start my-project
```

`session_list` discovers active sessions. Every other public tool requires `session_id`; use `session_info` to inspect the working directory, permitted roots, and whether the session is in yolo mode.

Relative paths resolve from the session working directory.

## Permission roots

A normal session starts with its startup directory as the permitted root. Change roots in the session terminal:

```text
/permission allow ../another-project
/permission revoke ../another-project
/permission list
/permission status
```

Normal sessions reject paths, symlink targets, and command working directories that escape the permitted roots.

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

Switch a running session with `/permission ask` or `/permission yolo`.

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
