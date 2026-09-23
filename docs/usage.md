# Using Temote MCP

[日本語](usage.ja.md)

## Sessions

For local work, create the session directly. If the lifecycle supervisor is not already running, the local CLI starts the exact current Temote binary as the supervisor and waits until its control socket is ready. New sessions default to sandboxed, approval-free `agent` mode:

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp session start my-project --path src/my-project
temote-mcp session list
temote-mcp session info my-project
```

Use `temote-mcp session console` when local approval input is required. Closing that console or sending stdin EOF detaches it without stopping the runtime. While no console is attached, approval-required operations fail closed.

Use `temote-mcp activity [SESSION_ID] [--tail N] [--no-follow]` to inspect the supervisor's bounded local activity stream. It follows by default, uses a filtered tail of 100, and is a best-effort diagnostic rather than a durable audit log. See [Managed sessions and named roots](managed-sessions.md#local-activity-viewer) for privacy, loss, retention, and disconnect behavior.

After replacing the installed binary, run `temote-mcp upgrade --dry-run` and then `temote-mcp upgrade` for a compatible same-PID supervisor handoff with coordinated session restart/restore. No credential values are persisted; missing restart context or an in-flight operation aborts the transition, and every planned session is verified before success. A supervisor from before the handoff protocol needs one manual restart first.

`session list` includes durable `starting`, `active`, `stopping`, `stopped`, and `crashed` states, plus `degraded` for a session whose durable metadata is intact but whose canonical working directory or a permitted workspace root no longer resolves. A degraded entry keeps its stored ID, path, and lifecycle timestamps instead of failing the whole listing; the missing path is never treated as a stopped or crashed runtime, and `session info` returns the same bounded degraded view. `session info` includes the working directory, permitted roots, permission mode, timestamps, exit reason, and last error. When the working directory is inside a supported standard Git worktree, the view also includes a non-secret `workspace` identity (`workspace_type` of `canonical_checkout`, `managed_worktree` or `legacy_worktree`, plus `repository_root`, `workspace_root`, `repository`, `branch` and managed-task name when known) derived from the configured `src` named root; the identity is read-only and disappears when the workspace no longer resolves. A dead or ambiguous socket is never silently treated as active. Manual restart is available with `temote-mcp session restart <id>`; automatic restart is not enabled. Restart fences the old full session instance, shuts down its registered Codex runtimes before starting the replacement, and does not leave those child runtimes running if replacement startup fails.

Session discovery is active-first: sessions owned by the running supervisor are returned before bounded historical metadata, so accumulated history cannot evict active sessions from `session list` / MCP `session_list`. Historical stopped/crashed entries are returned in a deterministic recent-first order within the list budget; historical metadata whose workspace no longer resolves is omitted from that bounded history (its metadata is retained, and `session info` still returns the degraded view for it). Supervisor startup and periodic maintenance retain the 512 most recent safely confirmed terminal metadata pairs and prune only older confirmed stopped/crashed pairs. Live, ambiguous, malformed/orphaned, and supervisor-upgrade restore-plan-protected metadata is never automatically removed by retention; read-only listing and MCP fallback do not perform cleanup.

Use `temote-mcp session forget <id>` to remove Temote-owned durable state for one terminal, non-live session: its metadata, lifecycle state, and a confirmed-stale socket entry. `stop` keeps that metadata for later `session list` / `session info`; `forget` intentionally removes it. The command refuses an unconditional live runtime socket probe, is serialized with supervisor lifecycle transitions, rejects symlink or non-regular metadata targets, and never touches the workspace, cwd, or worktree. Removing one session does not change the retention policy for other sessions.

`temote-mcp session gc` is the bounded maintenance path for accumulated orphan metadata halves. It defaults to a dry-run plan (`--apply` performs deletion) and accepts `--limit 1..=1000` (default 100). Only the initial reviewed classes are eligible: a lone `.state` lifecycle half (`missing_json`) or a lone `.json` metadata half (`missing_state`) that is older than a 24-hour grace period, is a regular non-symlink file, is not supervisor-owned or upgrade-protected, does not answer a live session socket probe, and (for lone metadata) is readable with a matching session ID. Everything else — malformed, mismatched, symlinked, special-file, and in-grace entries — is reported but never deleted. The plan is ordered oldest-first for a deterministic bounded limit, and `--apply` revalidates each candidate immediately before deleting, skipping any entry that drifted or was started concurrently. Cleanup is limited to Temote-owned session metadata files and never touches workspaces, cwd, or worktrees.

For compatibility, `cd ~/src/my-project && temote-mcp start my-project` starts the current directory and bootstraps the same local supervisor when needed. `temote-mcp start my-project --yolo` remains the deliberately unrestricted local-only form.

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

`session_stop` can stop only sessions marked as HTTP-owned by the lifecycle supervisor; it cannot stop local CLI/yolo sessions even though they share the same supervisor process. HTTP managed sessions are always non-yolo and default to `agent`, so the normal structured development workflow does not need a local approval console; an operator can still request the stricter `ask` mode locally with `session permission`. Public session-bound tools also reject separately started yolo sessions, so remote access cannot inherit their unrestricted local semantics. Stopped/crashed metadata remains visible through `session_list` / `session_info`; ordinary session-bound tools still require an active socket. `temote-mcp down` stops only the HTTP origin and its managed ingress child, not the lifecycle supervisor or its sessions. In a repository checkout, `just up/down` are development wrappers around these installed-binary commands.

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

Named roots come from `TEMOTE_MCP_ROOTS` on the host before Temote MCP starts. Set a single mapping such as `TEMOTE_MCP_ROOTS='src=~/src'` or a JSON object such as `TEMOTE_MCP_ROOTS='{"src":"~/src","opt":"~/opt"}'`, then restart Temote MCP. When it is unset, `session_start` stays disabled and named-root resolution errors explain how to configure it. A running session's roots can also grow through host-approved `directories` grants (below) without a restart.

The legacy inline `/permission ...` terminal command UI is not the owner of detached runtimes and is not exposed through the first supervisor control surface. This does not widen permissions: the runtime remains fail-closed with its persisted permitted roots.

## Permission modes

An explicit session permission mode controls the Temote-local approval layer:

- `ask` keeps the strictest policy: sandbox and path containment stay in force, and host/network-sensitive structured operations require the local approval console.
- `agent` is the default for newly created sessions, including authenticated public `session_start`. It keeps the same sandbox, path containment, and tool-specific validation, and its ordinary `execute`/`start_command` runs use the network-enabled development sandbox profile, but it does not require the local approval console for otherwise-valid structured operations: Git fetch/pull/branch-push/tag-push, `local_agent_run`, `dev_tool_run`, checkpoints, patches, the structured 1Password/kintone integrations, and the Codex app-server / OpenCode serve delegation tools (`codex_status`, `codex_task_*`, `opencode_status`, `opencode_task_*`).
- `yolo` remains the local-only unrestricted mode and cannot be created or promoted through public HTTP.

`agent` is not a weaker spelling of `yolo`: ordinary `execute`/`start_command` remain sandboxed and path-contained (`ask` restricted, `agent` development-network-enabled), public `without_sandbox` remains unavailable, force-push and arbitrary Git URLs/refspecs remain rejected, and integrations keep their own authentication and capability boundaries.

Use `temote-mcp session permission <id> status|ask|agent|yolo` to inspect or intentionally change a running managed session. Existing persisted sessions keep their stored mode across restart, automatic restart, restore, and upgrade handoff; an explicit `ask` session is not silently migrated to `agent`.

## Session capability grants

A running sandboxed session can request additional, individually scoped capabilities through `session_permission_request({session_id, listen_ports?, dev_tool_env_prefixes?, ambient_git_credentials?, directories?})`. Every field is additive and every non-empty field crosses the local approval console — the host sees the exact ports, prefixes, and paths before approving. Bundling fields collects them under one approval prompt, so an automation can gather several capabilities in a single host interaction. Approved grants persist in session metadata, survive session restart, and are listed by `session_info` under `grants`. The local CLI equivalent is `temote-mcp session permission <id> grant|ungrant` with the corresponding options; removing a permitted directory still uses `session permission revoke <path>`.

- `listen_ports: [5173, ...]` (up to 64) lets a command bind TCP listeners on exactly those ports, but only when the call also sets `allow_loopback_listen: true` on `execute` or `start_command`. On macOS the sandbox cannot scope a bind to loopback, so a granted port can bind on every interface — grant only the ports the workload needs. On Linux the development profile already permits listening, so the option is a no-op. `port_check({session_id, port})` probes `127.0.0.1:<port>` from the host and reports whether it accepts connections; only granted ports may be probed, keeping it a workspace observation tool rather than a general port scanner.
- `dev_tool_env_prefixes: ["MADOBE_", "CARGO_"]` (up to 32 prefixes of `1..=64` bytes using only `[A-Za-z0-9_]`) lets `dev_tool_run` accept an `env` object whose names start with a granted prefix. Names and values are bounded and cannot contain NUL, and broker-set variables such as `npm_config_ignore_scripts` still cannot be overridden.
- `ambient_git_credentials: true` lets validated `git_fetch`, `git_pull`, `git_push`, and `git_push_tag` fall back to ambient host Git credentials (credential helpers and the forwarded `ssh-agent`) when the repository has no managed GitHub credential mapping. A configured mapping still takes precedence, and global `gh` auth state is never touched.
- `directories: ["/abs/path", ...]` (up to 16, absolute paths) extends the session's permitted roots mid-flight.

Keep the original startup-directory rule: add directories through host approval rather than starting sessions with broad roots.

## Commands

`execute` runs argv without a shell. In `ask` sessions it runs inside Temote MCP's sandbox with network disabled; in the default `agent` mode it keeps the same sandbox and path containment but uses the network-enabled development profile for localhost/LAN/Internet development traffic. `yolo` remains the local-only unrestricted host path. If the command completes within the foreground timeout, the result is returned immediately; otherwise it returns a `job_id`.

Use `start_command` when work should be backgrounded immediately, then `poll_job` until completion or `stop_job` to cancel it. Jobs belong to their session, have a two-hour lifetime limit, and are cancelled when the session stops. A session can have up to eight active sandbox jobs.

`job_list({session_id, limit?})` returns a redacted snapshot of the current session's in-memory jobs. It reports only `job_id` and `running` / `completed` / `failed` / `unknown`, with running jobs first and a `truncated` flag. It never returns command text, argv, stdout/stderr, or raw errors, and listing does not consume a completed result. `retention="in_memory"` is explicit: an empty list is not proof that no work ran before restart or cache expiry.

The combined stdout/stderr retained for a command is capped at 1 MiB and reports when output was truncated.

### Experimental Codex tasks

The opt-in `codex_status`, `codex_task_start`, `codex_task_get`, and `codex_task_control` tools connect to a local `codex app-server --stdio` and accept only the named status/task operations. Temote does not pin compatibility to a Codex app-server version string. The initialize response is bounded and shape-checked, the app-server version is reported only as best-effort diagnostic metadata when it can be parsed, and compatibility is enforced by validating the concrete `model/list`, `thread/*`, and `turn/*` requests and responses that Temote actually uses. A task is owned by the complete session instance and its canonical working directory, so it cannot be resumed from another session, process generation, or scope.

`codex_task_start` and `codex_task_control` require an opaque `operation_id`; control actions are typed `steer`, `resume`, and `interrupt`. Typed `resume` reconciles the retained thread and turn; it does not start a new turn or revive a terminated child process. Temote persists an accepted receipt before starting or controlling the child turn; an uncertain crash returns `reconciliation_required` instead of replaying a side effect. In `ask` mode these operations require local approval; `agent` and `yolo` sessions skip the Temote-local prompt. Approval details identify Codex provenance, operation/tool, target and scope, mutation/read-only status, and safe model/effort or command/file-change summaries. Prompts, control input, transcripts, raw command arguments, patch bodies, and command output are not placed in task metadata or approval/activity summaries. `codex_task_get` exposes only bounded, expiring, session-and-scope-bound evidence through an opaque `evidence_id`.

Temote yolo changes only Temote's own local sandbox and approval behavior; it does not authorize Codex child mutations. Codex app-server command and file-change approval requests keep the child approval boundary and fail closed when the user-approval transport is unavailable. Pre-thread initialization or model-list failures are reported as `retryable_failed` and the same start operation can be retried; after a thread/start or turn/start request may have been sent, replay remains `reconciliation_required`. `codex_task_get` reconciles the remote thread before applying `after_revision`/`not_modified`. When another Temote process owns the live app-server runtime, it returns the persisted revision with `reconciliation_deferred: true` and does not start a competing resume; typed control fails before its operation receipt is persisted. While that lease remains live, session cleanup does not overwrite the task; the runtime owner shuts down and finalizes it after observing session termination. Task records are retained for the full task-retention period, including unexpired terminal records; only expired terminal records without a live child runtime may be pruned. A scope at its retention limit rejects a new start instead of deleting an unexpired record, and compacted operation receipts retain exact-replay conflict protection during retention.

Generated turns are requested with Codex `workspaceWrite`, the session's canonical directory as the writable root, and network access disabled. This is an experimental app-server adapter, not the same OS-level boundary as Temote's direct `execute` sandbox: the app-server process itself communicates with the inference service outside that direct command sandbox. It exposes no generic JSON-RPC, remote shell, or automatic approval path. If the installed Codex build or its sandbox behavior cannot be validated, keep these surfaces disabled/opt-in.

### Experimental OpenCode tasks

The opt-in `opencode_status`, `opencode_task_start`, `opencode_task_get`, and `opencode_task_control` tools spawn a per-task `opencode serve` child on loopback and talk to it through the `unofficial-opencode-sdk` HTTP client. Each serve child runs on 127.0.0.1 with a dynamically assigned port, a per-instance random Basic-auth password passed only through the child environment, an isolated per-task data directory (seeded with a private copy of the host's OpenCode auth), and a bounded serve permission configuration injected through `OPENCODE_CONFIG_CONTENT`. Task records, ownership, leases, receipts, retention, and scoped evidence follow the same contract as the Codex app-server tasks above: a task is owned by the complete session instance and its canonical working directory and cannot be resumed from another session, process generation, or scope.

`opencode_task_start` and `opencode_task_control` require an opaque `operation_id`; control actions are typed `steer`, `resume`, and `interrupt`. Steer sends another prompt on the retained session; resume reconciles the retained session and respawns a fresh serve child against the same per-task state directory — it does not start a new task or revive a terminated child. Prompts carry a deterministic `messageID` derived from the operation, so `opencode_task_get` can tell whether a start prompt was ever admitted before declaring `reconciliation_required`. Terminal-state reports are extracted from the last assistant message under the same bounded report contract as `local_agent_run`; usage and the observed model are read from session messages rather than self-reported values. In `ask` mode they require local approval with the same provenance/scope/mutation metadata as Codex tasks, while `agent` and `yolo` sessions skip the Temote-local prompt; `opencode_task_get` exposes only bounded, expiring, session-and-scope-bound evidence.

This is an experimental serve adapter, not the OS-level boundary of Temote's direct `execute` sandbox: the serve process itself communicates with provider APIs outside that direct command sandbox. Pending OpenCode permission/question requests surface as `waiting_approval` task state rather than auto-approval. Keep these surfaces opt-in until the host's installed OpenCode build has been validated end-to-end.

OpenCode 1.x and 2.x expose different HTTP contracts (`global/*` vs `api/*`). At spawn time Temote probes the `global/health` route first and keeps the established 1.x code path whenever it answers; only a serve child that cannot satisfy the 1.x contract falls through to the `api/*` (2.x) adapter, so an installed build that serves both still resolves deterministically to 1.x. Set `TEMOTE_OPENCODE_SERVE_CONTRACT` to `v1` or `v2` to skip auto-detection and pin a contract; any other value (or the variable being unset) keeps auto-detection.

### Structured local agent broker

`local_agent_run({session_id, agent, task, cwd?, worktree?, access, model?, effort?, profile?})` runs one locally installed Codex or OpenCode agent through a structured broker. `model` selects the adapter model identifier, `effort` is a simple Codex reasoning-effort name (Codex only, rejected for other agents), and `profile` selects the named provider/auth profile used for the child. `agent` is limited to `codex` and `opencode`; the caller supplies a bounded task and an access mode, not an executable, raw argv, environment, or network policy. Temote constructs the adapter-specific command line and verifies the installed non-interactive CLI contracts before shipping this feature. With `worktree: {branch, task?}` the caller supplies no path at all: Temote resolves the canonical repository from the selected session workspace, derives `<configured src root>/worktrees/<repo>/<task>` itself, reuses only a verified managed worktree of that repository on that branch, and otherwise creates one through the approved managed-worktree path. `cwd` combined with `worktree` is rejected, and the validated workspace is re-verified immediately before the agent starts. Legacy worktrees such as `<repository>/.wt/<name>` or `<src>/<repo>-*` are never adopted, moved or deleted by this selection.

The optional `cwd` is canonicalized and must remain inside a permitted session root, including after symlink resolution, even when the session is yolo. Permitted roots authorize which `cwd` may be selected; they are not an automatic list of paths exposed to the child. Only the selected canonical `cwd` is re-exposed as the agent workspace: it is writable for `workspace_write` and read-only for `read_only`. Other permitted roots are not automatically exposed to the agent. Private per-run state/cache remains writable in either mode, and every `.git`, `.agents`, and `.codex` entry found under the selected workspace is protected.

In `ask` and `yolo` modes every local-agent request crosses the local approval boundary. In `agent` mode an otherwise-valid structured request skips only the Temote-local approval prompt; the broker contract below is unchanged. A denial is returned before the child process is started. Codex tasks are limited to 1 MiB and are delivered through stdin using the verified `codex exec ... -` contract, so the task body is not placed in argv. The installed OpenCode `run [message..]` contract has no verified stdin prompt transport, so its positional message is limited to 64 KiB. Combined child output is limited to 1 MiB, and work longer than the foreground timeout returns a normal Temote `job_id` that can be inspected with `poll_job` or cancelled with `stop_job`. The interactive approval detail shows a bounded, control-sanitized task preview; durable activity and metadata retain only the agent, scope, access mode, task byte count, and SHA-256, never the task body, preview, or environment values.

The child environment is cleared and rebuilt from a small allow-list, so Temote-held credentials, tokens, and proxy settings are not forwarded implicitly. The outer profile hides the host temporary and user-agent state roots, then re-exposes only the selected workspace, executable directories, and private run state needed for this invocation. Existing Codex (`~/.codex/auth.json`) and OpenCode (`~/.local/share/opencode/auth.json`) login files are imported as bounded read-only inputs into the private run state for the top-level agent runtime. The broker supplies Codex's strict permission profile and OpenCode's read/external-directory restrictions so model-generated command/tool execution cannot read the imported auth file; the original user files are hidden and never writable by the child. Agent edits are not Git remote authorization; use the dedicated `git_*` tools for staging, commits, fetch, pull, and push. Public HTTP exposes only this structured broker and continues to omit the generic `without_sandbox` tool.

### Structured developer tool broker

`dev_tool_run({session_id, tool, operation, args?, cwd?})` runs a validated Cargo, Vite+, uv, npm, pnpm, or Go operation through the developer broker. Callers cannot select an executable or supply a raw host command. The cwd is canonicalized inside the permitted session roots, child output is bounded, and long operations return a normal `job_id`.

Operation classes:

- offline development (`cargo fmt|check|clippy|test|build`; `vp check|lint|fmt|format|test|build|pack`) run in the developer sandbox with network disabled; workspace write plus narrowly scoped tool cache/state write only;
- dependency/network (`cargo fetch|install|update`; `vp install|add|update|outdated|info|rebuild`) use the explicit network profile with the same scoped writes;
- the initial package-manager slice adds `uv lock`, `npm install|ci|update|ping|outdated`, `pnpm install|fetch|update|outdated`, and `go mod_download` (rendered as `go mod download`). These operations accept no caller-supplied arguments yet; `uv lock` forces `--no-build --no-python-downloads`, npm/pnpm install/update paths force lifecycle scripts off with `--ignore-scripts` and `npm_config_ignore_scripts=true`, and pnpm install/update/fetch also force `--ignore-pnpmfile` so project hooks cannot execute;
- lifecycle build scripts are a separate offline phase: `npm rebuild` and `pnpm rebuild_pending` (rendered as `pnpm rebuild --pending`) run with the developer sandbox network disabled. This lets a dependency fetch/install complete with scripts disabled first, then executes required build scripts without giving those scripts outbound network access;
- `vp run|exec|dlx`, `vp upgrade|implode`, and every unknown operation stay rejected rather than entering an offline/safe path.

In `ask` mode a validated operation requires local approval; in `agent` mode it runs without the local approval console; in `yolo` mode the existing local behavior is unchanged. The classification and containment rules are identical in every mode.

### Delegation backend (local CLI)

`temote-mcp delegate --backend codex|opencode ...` runs one bounded, non-interactive delegation process from the local CLI and prints one bounded JSON result. OpenCode also accepts an explicit `--session <id>` for resume. Resume first performs a bounded, read-only `opencode session list --format json` preflight and requires the session's canonical directory to equal the current canonical delegation directory; missing, ambiguous, malformed, oversized, failed, or mismatched metadata fails closed before `run` starts. Adding `--fork` requires `--session` and starts a new session that inherits the named session's context; the same fail-closed directory preflight runs against the parent session before `run` starts. `--continue` and `--attach` are not supported. The OpenCode backend resolves its executable in this order:

1. `TEMOTE_OPENCODE_BIN`, when set, must be an absolute path to an existing executable regular file. Symlinks are resolved to their canonical target. It takes precedence over PATH.
2. otherwise `opencode` is resolved from PATH.

An explicitly configured but invalid `TEMOTE_OPENCODE_BIN` (empty, relative, missing, not a regular file, or not executable) fails closed; Temote does not silently fall back to a different PATH executable. The configured path is never printed in diagnostics or errors. `temote-mcp delegate diagnose --backend opencode` reports only `available`/`unavailable`, the source (`env_override`, `path`, or `invalid_override`), and a bounded reason for an invalid override. `TEMOTE_OPENCODE_BIN` is read by the parent process and is not passed to the OpenCode child environment.

## Work checkpoints and handoff

`checkpoint_save` stores a bounded client-reported checkpoint in Temote's private state, scoped to the session's current canonical working directory. Every save requires an opaque UUID `operation_id`. New checkpoints omit `checkpoint_id` and use `expected_revision=0`; updates supply the existing UUID and current revision. Retrying the same logical mutation with the same `operation_id` and identical canonical request returns the previously committed result without creating another checkpoint or revision. Reusing an operation ID with a different request fails with `OPERATION_CONFLICT`; an ordinary stale revision still fails with `CHECKPOINT_CONFLICT`. Operation receipts are persisted atomically with the checkpoint and retained in a bounded history. Normal sessions require local approval; yolo keeps the existing auto-approval semantics. `checkpoint_load` can read the record only from the same canonical working-directory scope, including from another session for that same worktree.

Checkpoint status and check results are always labeled `source="client_reported"`. Even a `verified` report is only consistency-checked against its reported checks and commit; Temote does not infer verification from command success. Do not put credentials, tokens, private command output, or other secrets in checkpoint title/description fields. Approval/activity summaries include only the tool and step/check counts, not free-form checkpoint text.

`work_handoff({session_id, checkpoint_id?})` is read-only. Without an ID it lists bounded same-scope checkpoint candidates without choosing one. With an ID it returns that checkpoint, a redacted `source="live_snapshot"` view of current-session jobs, `freshness="not_revalidated"`, resume hints, and best-effort `automatic_recall` built locally from the checkpoint title plus reported next-step description. Automatic recall uses the repo-managed `learnings/` index, requires no network, adds `review_recalled_learnings` only when hits exist, and never executes checkpoint text. The handoff still does not replay work, run Git, or validate artifacts.

A safe resume flow is: `session_info` → `work_handoff` → choose a checkpoint → `work_handoff(checkpoint_id=...)` → inspect/poll any running jobs before repeating work → separately revalidate Git state, artifacts, and checks → choose the next operation.

## Bounded multi-file patches

`apply_patch({session_id, patch})` accepts the Codex-style `*** Begin Patch` format with add, update, move, and delete operations. Temote parses the patch in Rust rather than executing the patch body through a shell. Before the first write it validates every source and destination, rejects absolute/traversal paths and symlink escapes, bounds patch/file sizes and operation count, and requires all paths to remain inside the session roots even in yolo mode. Normal sessions receive one local approval for the whole preflighted patch. Approval/activity metadata contains only operation counts, never the patch body.

Multi-file application is not advertised as transactionally atomic across independent files. If an I/O error occurs after one or more operations were committed, the result is `partial_failure` with a machine-readable `committed` list describing exactly which add/update/move/delete steps completed before the error. A malformed patch or any preflight failure writes nothing.

## Friction, learning candidates, and recall

Temote keeps a bounded owner-only friction event store containing only structured execution metadata: event/session IDs, canonical scope, enum kind/source/outcome, restricted operation/tool identifiers, and optional UUID links. It does not persist command argv, stdout/stderr, file contents, prompts, approval bodies, environment values, credentials, or transcripts. Current automatic emitters cover command/Git failures, ambiguous partial `apply_patch` mutations, and actual negative approval responses in normal sessions; runtime shutdown of a pending prompt is not misclassified as a user denial. `recall_feedback(outcome="no_hit")` can add an explicitly `client_reported` knowledge-gap signal without persisting the query or recall results.

`friction_summary({session_id})` is read-only and returns an explainable bounded score with per-kind counts and capped contributions. A clean session produces no candidate merely because it used many tools, repeated same-kind failures are capped, and a recall miss alone scores zero. `learning_candidate_list({session_id})` derives review-only candidates from that summary; it never publishes authoritative knowledge automatically and never copies checkpoint text, transcripts, or command output into a candidate.

Authoritative learnings are repo-managed Markdown. `recall({session_id, query, knowledge_root?, limit?})` defaults to the relative `learnings/` directory (or another explicitly supplied relative root inside the session roots), rebuilds a deterministic local index on every request, and requires no network, embedding API, or vector database. Each Markdown learning must have `title`, `date` (`YYYY-MM-DD`), one or more bounded `tags`, `domain`, `verification`, plus `## Problem`, `## Resolution`, and `## Reusable lesson` sections. Recall returns the matched/missing terms and score for each hit. Publishing or editing a learning continues to use the existing `write_file` and Git trust/approval flow.

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
- `git_push_tag` pushes one exact local commit SHA to `refs/tags/<tag>` on a configured remote. Without `expected_remote_sha` it is create-only; with an expected SHA it updates only if the remote tag still equals that exact old SHA. It creates lightweight remote tag refs only and exposes no arbitrary refspec, URL, annotated-tag creation, or unconditional force.
- `git_branch_create` creates one validated local branch from `HEAD` or a validated repository-local/fetched ref. It does not switch the current worktree and exposes no force/reset/refspec/URL input.
- `git_branch_delete` deletes one exact local branch with merged-only semantics. It refuses the current branch, a branch checked out in any worktree, an unmerged branch and an absent branch; no force-delete option is exposed.
- `git_remote_branch_delete` deletes one exact `refs/heads/<branch>` from one configured push destination only when the caller's reviewed `expected_remote_sha` still matches. The live remote symbolic `HEAD` protects the default branch. GitHub destinations require live branch metadata to report `protected=false`; other destinations require a valid repository-local `temote.remote.<remote>.protectedBranch` policy. Missing or ambiguous default/protection state fails closed. Temote constructs the delete ref and exact force-with-lease internally; arbitrary URLs/refspecs, wildcards, tags, multiple push destinations and unconditional force are unavailable.
- `git_switch` switches to one validated existing local branch without force/reset/stash. If dirty files would be overwritten, Git rejects the switch and Temote leaves the worktree unchanged.
- `git_worktree_add` creates a linked worktree only at `<repository>/.wt/<name>`. Supplying `base` creates the validated branch from that repository-local commit; omitting `base` attaches an existing validated local branch. Arbitrary destination paths and force options are unavailable.
- `git_worktree_create` creates a linked worktree only below the selected repository's exact managed root, `<configured src root>/worktrees/<repository>/<task>` (which is `~/src/worktrees/<repo>/<task>` for the usual `TEMOTE_MCP_ROOTS='src=~/src'` layout). The canonical repository must be exactly one directory below the configured `src` named root, and only one validated existing local branch can be attached; create a new branch with `git_branch_create` first. `task` is optional and otherwise derived from the branch with `/` flattened to `-`; callers can never supply a filesystem path, `cwd` or `base`. Everything before the local approval is read-only, and a successful create is re-verified against the trusted managed root and repository identity before it is reported as created. Absolute paths, traversal, separators, option-like values, control characters, symlink escapes and every existing target are rejected. Legacy worktrees such as `<repository>/.wt/<name>` and `~/src/<repo>-*` are never moved, adopted, reused or deleted.
- `git_worktree_list` lists the selected repository's registered worktrees with a `primary`, `managed` or `legacy` classification. `managed` requires canonical containment below a trusted managed root (the exact configured `src` root as a normal directory, never a symlink or swapped path) plus a matching canonical common Git directory and primary checkout; anything unverifiable fails closed as `legacy`. The listing is read-only and does not change any worktree.
- `github_workflow_dispatch` resolves the GitHub repository only from the selected configured `github.com` remote, dispatches one numeric workflow ID or `.yml`/`.yaml` workflow filename at one exact unqualified branch/tag ref, and returns the created workflow run ID. It requires the repository-local Git credential mapping to explicitly reset helpers and select `!gh git credential --managed` with `credential.useHttpPath=true`; there is no fallback to the ambient active `gh` account. After approval, Temote resolves that exact repository credential internally and uses it only for a bounded direct GitHub REST request. Inherited `GH_TOKEN`/`GITHUB_TOKEN`-style variables remain classified as sensitive, global `gh auth` state is not mutated, and no token value is returned.
- `github_workflow_run_get` reads bounded status for one exact workflow run ID in that same configured GitHub repository using the same repository-scoped credential mapping. Poll this tool until `status` is `completed`, then use `conclusion` as the terminal result. It does not download raw logs or artifacts.

Remote Git operations are host operations. In `ask` mode they require local approval; in `agent` mode the validated structured operation runs without the local approval console, and in `yolo` mode the existing local behavior is unchanged. The safe-remote, fast-forward-only, current-branch, and no-force rules are identical in every mode. `git_worktree_create` is the corresponding host-side workspace operation, and its managed-root policy and validation are identical in every mode.

GitHub HTTPS remotes require the repository-local managed credential mapping before `git_fetch`, `git_pull`, `git_push`, `git_push_tag`, and the `github_workflow_*`/`github_pr_*` tools may run. Configure it once per clone on the host:

```sh
git config --local credential.helper ''
git config --local --add credential.helper '!gh git credential --managed'
git config --local credential.useHttpPath true
```

The `credential mapping is unavailable` error repeats these steps. As an opt-in alternative, the host-approved `ambient_git_credentials` session grant lets the same tools fall back to ambient Git credentials when no mapping exists; non-GitHub and SSH remotes keep their normal credential path in every case.

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

## Remote upgrade and reconnect

Authenticated direct HTTP exposes `upgrade_preflight`, `upgrade_apply`, and
`upgrade_status`. These tools are intentionally absent from stdio MCP and the
multi-host gateway. Preflight and status are read-only host lifecycle calls.
Apply accepts only an active managed normal `session_id` and an optional
`expected_version`; it never accepts an executable path, URL, command, argv, or
environment. Ask and agent sessions both require explicit approval from the
local user, and public yolo sessions are rejected.

After `upgrade_apply` returns a newly accepted transaction, Temote closes that
HTTP connection and a detached local coordinator owns the remaining work. The
client should reconnect to the same configured endpoint with normal
authentication, verify the host/version/boot identity from initialize or ping,
and call `upgrade_status(transaction_id)` until it is terminal. Temote cannot
force an arbitrary MCP client to reconnect. A successful status means the
coordinator verified the target version, stable host identity, session restore,
and a new boot generation when ingress replacement was required.
