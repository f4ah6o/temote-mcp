---
name: temote-mcp
description: Operate local files, commands, Git, background jobs, 1Password, and kintone through Temote MCP. Use when the user mentions Temote MCP or temote-mcp, asks an agent to work on a local repository/session through Temote, supplies a Temote session ID, or when tools such as session_list, session_info, execute, git_commit, or git_push are available.
license: MIT AND Apache-2.0
compatibility: Requires an MCP connection to a running Temote MCP server and at least one local session for tools that use session_id.
metadata:
  author: f4ah6o
---

# Use Temote MCP effectively

Temote MCP exposes a user's local machine through explicit sessions. Treat the selected session as the source of truth for its working directory, permission mode, filesystem roots, and host process state.

## Select the session first

1. If the user explicitly names a session ID, use that exact ID.
2. Otherwise call `session_list` before asking the user for an ID.
3. Match the user's target project to the returned `cwd`. If one session clearly matches, use it.
4. Call `session_info` before operations where the working directory, permitted roots, or yolo mode materially affects the action.
5. Do not silently switch to a different session midway through a task.

Do not ask the user to repeat a session ID or path that Temote MCP can discover itself.

## Inspect before modifying

For repository work, establish current state before editing:

- use `list_directory` and `read_file` for focused file inspection;
- use `execute` for read-only commands such as `git status --short --branch`, `git log`, searches, tests, and build commands;
- inspect existing issue/design files when the task refers to them;
- avoid repeating diagnostics whose current result is already available and still relevant.

Relative paths are resolved from the session `cwd`.

## Files

Use the narrowest tool that fits:

- `list_directory` for directory contents;
- `read_file` for UTF-8 text;
- `get_image` for supported local images;
- `write_file` for UTF-8 edits.

In normal sessions, stay within permitted roots. If a required path is outside them, report the concrete path boundary instead of attempting a symlink or path traversal workaround.

## Commands and jobs

Use `execute` for normal commands. It takes argv, not a shell command string. Normal sessions run commands in the Temote MCP sandbox with network disabled; yolo sessions run with the local user's host permissions.

If `execute` returns a `job_id`, the work is still running. Poll it with `poll_job` until it finishes when completion is needed for the user's current task. Use `start_command` when backgrounding immediately is intentional. Use `stop_job` when the running command is no longer needed or must be cancelled.

Do not tell the user that work is complete while a required job is still running. Do not ask the user to wait instead of polling a job that can be completed in the current turn.

## Git

Use ordinary `execute` for read-only Git inspection. Use Temote MCP's dedicated tools for Git metadata writes and remote synchronization:

1. `git_add` with explicit paths.
2. `git_commit` with the intended commit message.
3. `git_fetch` or `git_pull` when remote updates are required.
4. `git_push` after local validation when the user requested pushing.

`git_pull` is fast-forward-only. `git_push` does not expose force push or arbitrary URL/refspec input. Do not bypass these restrictions with a shell command.

Before committing, inspect the diff/status and run the task-relevant checks. After pushing, verify the branch is synchronized when practical.

## Approval model

Normal sessions use Temote MCP's local approval boundary for host/network-sensitive operations. Yolo sessions intentionally skip Temote MCP approval prompts and path/sandbox restrictions.

Do not add a redundant conversational confirmation for an operation the user already explicitly requested merely because Temote MCP may also display its own host approval UI. Still follow any confirmation or authorization rules imposed by the current agent/client.

Never infer that yolo mode disables authorization outside Temote MCP.

## Network behavior

Normal `execute` commands have no network access. Prefer dedicated network-aware tools such as `git_fetch`, `git_pull`, and `git_push` for supported operations.

`without_sandbox` may exist only on local stdio and requires host approval in normal mode; it is not available on the public HTTP endpoint. Do not depend on it being present.

## 1Password bridge

Use the official child MCP bridge discover-first:

1. `onepassword_mcp_discover`
2. `onepassword_mcp_read_resource` when its advertised documentation is needed
3. `onepassword_mcp_call` for a discovered child tool

Do not invent child tool names or schemas. Keep secret values out of summaries and user-visible diagnostic text unless the user explicitly supplied and requested those exact values.

For service-account workflows, use `onepassword_service_account_status` before assuming a token exists. `onepassword_service_account_run` accepts `op://...` references and checked-in env templates; do not replace secret references with plaintext.

## kintone bridge

Prefer the official MCP server for structured kintone operations:

1. `kintone_mcp_status`
2. `kintone_mcp_discover`
3. `kintone_mcp_call` with a discovered tool name/schema

Use `kintone_cli_status` and then `kintone_cli_run` when cli-kintone covers a gap better: attachment-aware bulk record export/import, guest-space record work, customization export/apply, or plugin upload. Pass CLI arguments without connection/authentication flags; those values belong to the `temote-mcp start` environment. Use `stdout_path` for large record exports instead of relying on captured stdout.

Do not guess tenant credentials or expose them. In normal sessions, forwarded kintone MCP calls and all cli-kintone runs are approval-gated.

## Failure handling

When a tool fails:

- preserve the exact meaningful error and identify whether it is a session, permission-root, sandbox, network, executable/configuration, or command failure;
- inspect current session state before retrying if the failure could be caused by a stopped/replaced session;
- do not weaken sandbox or yolo settings as a troubleshooting shortcut;
- do not retry non-idempotent operations blindly;
- prefer a generic repository/product fix over a one-off workaround when implementing software changes.

## Completion

For implementation tasks, complete the requested lifecycle in the same turn when tools permit it: inspect, edit, test, review diff, commit, and push if the user requested push. Report concrete validation results, commit ID, push result, and any remaining limitation.
