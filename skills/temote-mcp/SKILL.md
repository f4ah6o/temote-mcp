---
name: temote-mcp
description: Operate local files, commands, Git, background jobs, 1Password, and kintone through Temote MCP. Use when the user mentions Temote MCP or temote-mcp, asks an agent to work on a local repository/session through Temote, supplies a Temote session ID, or when tools such as session_list, session_info, execute, git_commit, or git_push are available.
license: MIT AND Apache-2.0
compatibility: Requires an MCP connection to Temote MCP. A serve endpoint may create normal sessions from host-configured named roots; otherwise tools using session_id require an existing local session.
metadata:
  author: f4ah6o
---

# Use Temote MCP effectively

Temote MCP exposes a user's local machine through explicit sessions. Treat the selected session as the source of truth for its working directory, permission mode, filesystem roots, and host process state.

## Select or create the session first

1. If the user explicitly names a session ID, use that exact ID.
2. Otherwise call `session_list` first.
3. Match the target project to an existing session `cwd`. If one clearly matches, use it.
4. If no session matches and `session_start` is available, start one with the host's logical named-root path such as `src/project`. Do not invent an absolute host path, do not pass a yolo option, and do not retry an unknown root by weakening path constraints.
5. Call `session_info` after selecting or creating the session, before ordinary tools.
6. Do not silently switch to a different session midway through a task.

The normal remote workflow is `session_list` → `session_start` when needed → `session_info` → ordinary tools. `session_stop` may stop only sessions owned by the current `serve` supervisor; never use it to try to terminate a separately started CLI session.

Do not ask the user to repeat a session ID or logical path that Temote MCP can discover or that the current task already supplies.

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

For general item reads, prefer `onepassword_item_get`. Put all items needed for one step into a single `items` array instead of issuing separate reads; the bridge resolves exact IDs/titles, deduplicates them, and batches the official `op` fetch. Concurrent calls in the same session and `(account, vault)` scope are also micro-batched and fanned out by resolved item ID, but explicit batching is still preferable because not every transport delivers calls concurrently. Use `vault` or `account` only when needed to resolve scope. The returned payload may contain secrets, so do not echo it into diagnostics or approval summaries.

For `op://` field resolution on macOS, prefer `onepassword_secret_resolve` when an account name/UUID is known. Batch all references needed for one step. Temote reuses an official 1Password Desktop SDK client in an isolated sidecar and falls back to one batched official CLI invocation if SDK authorization is unavailable. Treat the returned string array as secrets and never echo it into logs or summaries. Desktop SDK authorization is separate from `op` CLI sign-in.

For service-account workflows, use `onepassword_service_account_status` before assuming a token exists. `onepassword_service_account_run` accepts `op://...` references and checked-in env templates; do not replace secret references with plaintext. Prefer `environment` / `env_files` when secrets are known before startup. Temote resolves those inputs before launching the target. On Linux, the supervisor disables peer process inspection when a service-account credential is present, upgrade/re-exec uses a sealed anonymous-FD credential handoff instead of a raw-token startup environment, and raw-token CLI calls fail closed unless the resolved `op` binary has the expected root-owned, non-writable setgid installation whose dedicated group is unavailable to the Temote user. Linux service-account targets run with a private PID namespace and private `/proc`, so they cannot inspect host credential-bearing processes. If a Linux child must resolve reviewed secrets later through its own `SecretReader`, pass only the required exact references in `allowed_locators`; Temote pre-resolves that exact set and exposes it through a process-tree-bound per-invocation broker without exposing `OP_SERVICE_ACCOUNT_TOKEN`. Do not request a broad locator set, and do not implement plaintext or interactive fallback when the resolver fails.

## kintone bridge

Prefer the official MCP server for structured kintone operations:

1. `kintone_mcp_status`
2. `kintone_mcp_discover`
3. `kintone_mcp_call` with a discovered tool name/schema

Use `kintone_cli_status` and then `kintone_cli_run` when cli-kintone covers a gap better: attachment-aware bulk record export/import, guest-space record work, customization export/apply, or plugin upload. Pass CLI arguments without connection/authentication flags; those values belong to the `temote-mcp start` environment. Use `stdout_path` for large record exports instead of relying on captured stdout.

Do not guess tenant credentials or expose them. In normal sessions, forwarded kintone MCP calls and all cli-kintone runs are approval-gated.

## Supervisor upgrades

`temote-mcp up` owns HTTP/ingress, not the session supervisor. When the user explicitly asks to apply an already-installed Temote binary to the running supervisor and local command execution is available, use `temote-mcp upgrade --dry-run` first and inspect the complete plan before `temote-mcp upgrade`. Do not substitute re-running `temote-mcp up` for a supervisor handoff.

The dry-run is the source of truth for the transition. Check supervisor compatibility, `blocked_sessions`, in-flight-operation blockers, and direct-ingress actions. It may also report an ingress blocker when the current ingress cannot be reconstructed safely, for example because required restart context was interactive-only. Do not proceed with the destructive upgrade while any blocker remains.

A successful upgrade performs a same-PID supervisor handoff, restores and probes the intended active-session set, and reports deterministic partial-state details if restore fails. When direct ingress is active, it is left untouched only when already healthy on the target binary; otherwise Temote restarts it from its durable non-secret recipe and requires `/healthz` to recover before reporting success. After the supervisor/session transition succeeds, Temote also reconciles the binary-owned Codex plugin transactionally; report any exact manual follow-up or client-restart requirement returned by the command rather than assuming a running client has reloaded the plugin.

Do not persist or reconstruct plaintext credentials to force a transition. A supervisor that predates the handoff protocol requires one manual supervisor restart before later compatible releases can use `upgrade`.

## Failure handling

When a tool fails:

- preserve the exact meaningful error and identify whether it is a session, permission-root, sandbox, network, executable/configuration, or command failure;
- inspect current session state before retrying if the failure could be caused by a stopped/replaced session;
- do not weaken sandbox or yolo settings as a troubleshooting shortcut;
- do not retry non-idempotent operations blindly;
- prefer a generic repository/product fix over a one-off workaround when implementing software changes.

## Completion

For implementation tasks, complete the requested lifecycle in the same turn when tools permit it: inspect, edit, test, review diff, commit, and push if the user requested push. Report concrete validation results, commit ID, push result, and any remaining limitation.