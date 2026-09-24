---
name: temote-mcp
description: Operate local files, commands, Git, background jobs, 1Password, and kintone through Temote MCP. Use when the user mentions Temote MCP or temote-mcp, asks an agent to work on a local repository/session through Temote, supplies a Temote session ID, or when tools such as session_list, session_info, execute, git_commit, or git_push are available.
license: MIT AND Apache-2.0
compatibility: Requires an MCP connection to Temote MCP. A serve endpoint may create normal sessions from host-configured named roots; otherwise tools using session_id require an existing local session.
metadata:
  author: f4ah6o
---

# Use Temote MCP effectively

Temote MCP exposes a user's local machine through explicit sessions. Treat the selected session as the source of truth for its working directory, permission mode (`ask`, `agent`, or `yolo`), filesystem roots, and host process state. New sessions default to the sandboxed `agent` mode.

## Select the host and session first

1. If `host_list` is available, treat the connection as a federated gateway. If the user explicitly names a host, use that exact `host_id`; otherwise call `host_list` before choosing among multiple machines.
2. If the user explicitly names a session ID, preserve that exact ID. Use `session_list(host_id=...)` when a host is selected.
3. Match the target project to an existing session `cwd`. If one clearly matches on the selected host, use it. The same `session_id` may exist on different hosts.
4. If no session matches and `session_start` is available, start one with explicit `host_id` when supported and a logical named-root path such as `src/project`. Do not invent an absolute host path, do not pass a yolo option, and do not retry an unknown root by weakening path constraints.
5. Call `session_info` with explicit `host_id` when supported after selecting or creating the session, before ordinary tools. Continue passing that `host_id` with session-scoped calls.
6. Do not silently switch to a different host or session midway through a task.

On a federated gateway, prefer `host_list` → `session_list(host_id=...)` → `session_start(host_id=...)` when needed → `session_info(host_id=..., session_id=...)` → ordinary tools. An unqualified `session_id` is a compatibility path only; if ownership is ambiguous, do not guess. `session_stop` and `session_restart` may act only on sessions owned by the selected host's public supervisor; never use them to try to control a separately started CLI/yolo session.

On a direct single-host Temote endpoint without `host_list`, keep using the existing session-only workflow.

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

Use `execute` for normal commands. It takes argv, not a shell command string. `ask` sessions run commands in the Temote MCP sandbox with network disabled; the default `agent` mode keeps the same sandbox and path containment with the network-enabled development profile; yolo sessions run with the local user's host permissions.

If `execute` returns a `job_id`, the work is still running. Poll it with `poll_job` until it finishes when completion is needed for the user's current task. Use `start_command` when backgrounding immediately is intentional. Use `stop_job` when the running command is no longer needed or must be cancelled.

Do not tell the user that work is complete while a required job is still running. Do not ask the user to wait instead of polling a job that can be completed in the current turn.

### Experimental Codex tasks

When the experimental `codex_status`, `codex_task_start`, `codex_task_get`, or `codex_task_control` tools are available, call `codex_status` first and treat its version/model/effort result as diagnostic compatibility metadata, not as a version allowlist or proof that a task will succeed. Start and control require a fresh UUID `operation_id`; preserve it for an exact retry and never retry an uncertain side effect with a new ID. Pre-thread startup failures may be retried with the same start ID; once a thread/turn request may have been sent, keep the task in reconciliation and do not blindly replay it. Use `codex_task_get` to reconcile remote truth, `reconciliation_required`, `unknown`, approval waits, and process restarts before deciding whether to control a task; it reconciles before honoring `after_revision`. If it returns `reconciliation_deferred: true`, another Temote process owns the live runtime: use the persisted status and revision, and do not issue control through the secondary process. A session stop/restart finalizes nonterminal tasks owned by the ended full session instance as `interrupted`; while another process holds the runtime lease, its owner observes session termination, shuts down the child, and finalizes the task. Retained records remain fenced from a replacement instance. Unexpired terminal records are not evicted to make capacity; when no expired terminal record is available, a new start is rejected. Temote yolo does not authorize Codex child mutations: command/file-change approval requests still use the explicit user-approval path and fail closed if it is unavailable. Control is limited to typed `steer`, `resume`, and `interrupt` actions, and compacted receipts remain conflict-protected during task retention. These tools expose named operations only—do not attempt to tunnel arbitrary app-server JSON-RPC or a remote shell. Prompts, transcripts, and raw child output are not a substitute for the bounded evidence reference returned by the task API. The app-server adapter is experimental and its generated-turn sandbox is not equivalent to Temote's direct command sandbox; do not claim normal `execute` guarantees for it without host-specific validation.

### Other delegation task backends

`opencode_status`/`opencode_task_*`, `devin_status`/`devin_task_*`, and `devin_cloud_status`/`devin_cloud_task_*` share the Codex task contract: call the matching `*_status` tool first, pass a fresh UUID `operation_id` on start, reconcile with `*_task_get`, and use only typed `steer`, `resume`, and `interrupt` control actions. OpenCode tasks drive a per-task `opencode serve` child on loopback; Devin tasks drive a per-task `devin acp` child over stdio ACP (`cloud: true` instead creates a hosted Devin Cloud session and ignores `model`/`agent`). Task details are exposed only through bounded, expiring `evidence_read` references. In `ask` mode starts and controls require local approval; in the default `agent` mode a valid structured operation skips only the Temote-local prompt. These are experimental opt-in surfaces: they are app-server/serve adapters, not the Temote `execute` sandbox, and they are not a substitute for a remote shell or arbitrary JSON-RPC tunneling.

### Structured developer tool broker

Use `dev_tool_run` for validated Cargo, Vite+, uv, npm, pnpm, or Go work instead of raw shell commands. Cargo/Vite+ keep their existing classified operations. The initial package-manager slice adds `uv lock`, `npm install|ci|update|ping|outdated`, `pnpm install|fetch|update|outdated`, and `go mod_download` (rendered as `go mod download`). These package-manager operations accept no caller-supplied arguments yet; `uv lock` forces `--no-build --no-python-downloads`, npm/pnpm install/update paths force lifecycle scripts off, and pnpm install/update/fetch also force `--ignore-pnpmfile` to disable project hooks. Run lifecycle build scripts only in the separate network-disabled phase: `npm rebuild` or `pnpm rebuild_pending` (`pnpm rebuild --pending`). Other package-manager script/exec operations remain rejected. Offline development operations run with network disabled and scoped tool cache writes; dependency/network operations use the explicit network profile. `vp run`, `vp exec`, `vp dlx`, `vp upgrade`, `vp implode`, unknown operations, and caller-selected executables or raw host commands are rejected. In `ask` mode a validated operation requires local approval; in the default `agent` mode it runs without the local approval console.

## Work state, checkpoints, and recall

`apply_patch` applies a Codex-style `*** Begin Patch` multi-file patch after a single approval and per-path preflight inside session roots; prefer it over piping diffs through `execute`. `checkpoint_save` persists a bounded client-reported work checkpoint scoped to the session working directory (supply a fresh `operation_id` for idempotent retries); `checkpoint_load` reads it back only for the same canonical cwd, and `work_handoff` projects a checkpoint together with a redacted live job snapshot for cross-session handoff. `friction_summary`, `learning_candidate_list`, and `recall`/`recall_feedback` surface bounded execution-friction scores, review-only learning candidates, and deterministic knowledge matches; none of them store or expose command output, prompts, or file contents.

## Git

Use ordinary `execute` for read-only Git inspection. Use Temote MCP's dedicated tools for Git metadata writes and remote synchronization:

1. `git_add` with explicit paths.
2. `git_commit` with the intended commit message.
3. `git_fetch` or `git_pull` when remote updates are required.
4. `git_push` after local validation when the user requested pushing a branch. Use `git_push_tag` only for an explicitly requested tag/release trigger, with the exact source commit SHA and an expected old SHA for updates.
5. Use `git_branch_create`, `git_branch_delete`, `git_remote_branch_delete`, `git_switch`, `git_worktree_add`, and `git_worktree_create` for branch/worktree mutations instead of raw `git` through `execute`. Local deletion is merged-only and rejects branches checked out in any worktree. Remote deletion requires the exact reviewed remote SHA and a single configured push destination; the live remote `HEAD` rejects the default branch, GitHub uses live `protected` branch metadata, and other remotes require a valid repository-local `temote.remote.<remote>.protectedBranch` policy. Missing or ambiguous state fails closed. `git_worktree_create` owns the Temote-managed `<configured src root>/worktrees/<repo>/<task>` destination (normally `~/src/worktrees/<repo>/<task>`, derived by Temote, never by the caller), and it attaches only an existing local branch, so create the branch with `git_branch_create` first. `git_worktree_list` classifies existing worktrees as `primary`, `managed` or `legacy`. `git_worktree_add` keeps the legacy `<repository>/.wt/<name>` contract; do not invent external worktree paths for either tool.
6. If the requested release uses GitHub Actions `workflow_dispatch`, use `github_workflow_dispatch` with the exact workflow/ref, preserve the returned run ID, and poll `github_workflow_run_get` until the run is terminal. Do not substitute `gh` through `execute`.

`git_pull` is fast-forward-only. `git_push` does not expose force push or arbitrary URL/refspec input. `git_push_tag` is limited to `refs/tags/<tag>` and uses exact `--force-with-lease` expectations: omission means the tag must not exist; an update requires the caller's exact expected remote SHA. Do not bypass these restrictions with a shell command. In `ask` these Git tools require local approval; in the default `agent` mode the validated structured operation runs without the local approval console; `yolo` keeps its existing local behavior.

Branch/worktree tools never expose unconditional `--force`, destructive reset, stash, arbitrary refspecs, or arbitrary destination paths. `git_branch_create` does not switch the current worktree. `git_branch_delete` is merged-only and rejects the current branch and every branch attached to a worktree. `git_remote_branch_delete` constructs only the exact `refs/heads/<branch>` deletion and binds it to `expected_remote_sha` with force-with-lease; its authoritative default/protection checks fail closed when unavailable, so do not use it without a reviewed remote tip and valid protection policy. `git_switch` targets only an existing local branch. `git_worktree_add` uses the legacy `<repository>/.wt/<name>` destination. `git_worktree_create` accepts no `cwd` or `base`: it derives `<configured src root>/worktrees/<repository>/<task>` only when the canonical repository is exactly one directory below the configured `src` named root, attaches an existing validated local branch (branch `/` becomes `-`), fails closed on traversal, separators, symlink escape or any existing target, and re-verifies the created path against the trusted managed root and repository identity. `git_worktree_list` reports managed worktrees only when the trusted root authority, canonical containment, common Git directory and primary checkout all verify, and reports everything else — including `.wt`, earlier sibling, `/tmp`, or wrong-repository worktrees — as `legacy` without adopting it. Preserve dirty/untracked/legacy work; do not reset, checkout-force, stash, move, or delete it as a workaround.

GitHub workflow tools do not use the globally active `gh` account. They require a repository-local managed Git credential mapping (`credential.helper` reset followed by `!gh git credential --managed`, with `credential.useHttpPath=true`) and fail closed when that mapping is absent. Never run `gh auth switch`, `gh auth login`, or `gh auth logout` to make a structured operation succeed. A raw `gh auth status` failure inside the normal sandbox is not authoritative evidence that the host repository credential is invalid.

Use `github_pr_list`, `github_pr_get`, and `github_pr_close` for pull-request inspection and closure under the same repository-local credential mapping; `github_pr_close` is a mutation and follows the same approval rules as other structured Git operations.

Before committing, inspect the diff/status and run the task-relevant checks. After pushing, verify the branch is synchronized when practical.

## Approval model

`ask` uses Temote MCP's local approval boundary for host/network-sensitive structured operations. The default sandboxed `agent` mode skips only that Temote-local prompt for otherwise-valid structured operations; it never widens sandbox, path, network, or tool-specific capability. Yolo sessions intentionally skip Temote MCP approval prompts and path/sandbox restrictions for ordinary operations.

A running sandboxed session can request additive capability grants through `session_permission_request({session_id, listen_ports?, dev_tool_env_prefixes?, ambient_git_credentials?, directories?})`; every non-empty field goes through the local approval console once, and granted values persist across session restart. `listen_ports` grants exact TCP listen ports used with `execute`/`start_command` `allow_loopback_listen: true` (a no-op on the Linux development profile, which already permits listen); `port_check({session_id, port})` reports whether a granted port accepts a host-side loopback connection and never acts as a general port scanner. `dev_tool_env_prefixes` grants bounded `env` entries on `dev_tool_run` for the exact granted name prefixes. `ambient_git_credentials` lets validated Git remote operations fall back to host Git credentials only when no repository-local managed mapping exists. `directories` extends the session's permitted roots mid-run; prefer it over restarting with a broader root.

Do not add a redundant conversational confirmation for an operation the user already explicitly requested merely because Temote MCP may also display its own host approval UI. Still follow any confirmation or authorization rules imposed by the current agent/client.

Never infer that yolo mode disables authorization outside Temote MCP.

## Network behavior

Normal `execute` commands have no network access. Prefer dedicated network-aware tools such as `git_fetch`, `git_pull`, `git_push`, `git_push_tag`, `git_remote_branch_delete`, `github_workflow_dispatch`, and `github_workflow_run_get` for supported operations.

`without_sandbox` may exist only on local stdio and requires host approval in `ask`/`agent` mode; it is not available on the public HTTP endpoint. Do not depend on it being present.

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

Do not guess tenant credentials or expose them. In `ask` mode forwarded kintone MCP calls and cli-kintone runs are approval-gated; in the default `agent` mode only the Temote-local prompt is skipped while kintone authentication, discovery gating, and argument validation remain enforced.

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
