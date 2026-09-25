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
- `agent` is the default for newly created sessions, including authenticated public `session_start`. It keeps the same sandbox, path containment, and tool-specific validation, and does not require the local approval console for otherwise-valid structured operations: the delegation tools (`codex_status`, `codex_task_*`, `opencode_status`, `opencode_task_*`, `devin_status`, `devin_task_*`, `devin_cloud_status`, `devin_cloud_task_*`).
- `yolo` remains the local-only unrestricted mode and cannot be created or promoted through public HTTP.

`agent` is not a weaker spelling of `yolo`: session scope, path containment, typed task contracts, and the delegated-agent approval/credential boundaries are identical; only the Temote-local approval prompt is skipped for otherwise-valid structured operations.

Use `temote-mcp session permission <id> status|ask|agent|yolo` to inspect or intentionally change a running managed session. Existing persisted sessions keep their stored mode across restart, automatic restart, restore, and upgrade handoff; an explicit `ask` session is not silently migrated to `agent`.

## Session capability grants

A running sandboxed session can hold additional, individually scoped capability grants persisted in session metadata and applied through the local `temote-mcp session permission <id> grant|ungrant` commands after host approval. Approved grants survive session restart and are listed by `session_info` under `grants`; removing a permitted directory still uses `session permission revoke <path>`.

- `directories` (up to 16, absolute paths) extends the session's permitted roots mid-flight. Keep the original startup-directory rule: add directories through host approval rather than starting sessions with broad roots.
- `ambient_git_credentials` lets the managed Git operations inside a delegated agent session fall back to ambient host Git credentials (credential helpers and the forwarded `ssh-agent`) when the repository has no managed GitHub credential mapping. A configured mapping still takes precedence, and global `gh` auth state is never touched.

## Delegation and jobs

Temote does not execute files, commands, Git, or integrations directly. Machine work is delegated to a coding agent on the local machine through the task backends below. Task transcripts and child output cross the boundary only as bounded, expiring, session-and-scope-bound evidence; read them with `evidence_read({session_id, evidence_id, offset_bytes?, max_bytes?})`.

Work that outlives the foreground timeout returns a session-owned `job_id`; poll it with `poll_job` until completion or cancel it with `stop_job`. Jobs belong to their session, have a two-hour lifetime limit, and are cancelled when the session stops.

`job_list({session_id, limit?})` returns a redacted snapshot of the current session's in-memory jobs. It reports only `job_id` and `running` / `completed` / `failed` / `unknown`, with running jobs first and a `truncated` flag. It never returns command text, argv, stdout/stderr, or raw errors, and listing does not consume a completed result. `retention="in_memory"` is explicit: an empty list is not proof that no work ran before restart or cache expiry.

The combined stdout/stderr retained for delegated work is capped at 1 MiB and reports when output was truncated.

Every task view returned by the delegation backends keeps the backend execution `status` and adds three separated states: `execution` (the stable id and generation of the current execution of the logical task), `verification` (`not_run` / `passed` / `failed`, bound to the task record revision the result was recorded against), and `delivery` (`not_started` / `pending` / `submitted` / `merged` / `closed` / `failed`). A `completed` execution is not a verification pass: until a result applies to the current revision, `verification.status` stays `not_run`, and a result from an older revision is reported as `stale: true` with its previous target instead of a current PASS. Delivery is set only by a recorded delivery operation; pull requests reported by an agent do not set it. Records written before these fields existed read as `not_run` / `not_started`.

### Experimental Codex tasks

The opt-in `codex_status`, `codex_task_start`, `codex_task_get`, and `codex_task_control` tools connect to a local `codex app-server --stdio` and accept only the named status/task operations. Temote does not pin compatibility to a Codex app-server version string. The initialize response is bounded and shape-checked, the app-server version is reported only as best-effort diagnostic metadata when it can be parsed, and compatibility is enforced by validating the concrete `model/list`, `thread/*`, and `turn/*` requests and responses that Temote actually uses. A task is owned by the complete session instance and its canonical working directory, so it cannot be resumed from another session, process generation, or scope.

`codex_task_start` and `codex_task_control` require an opaque `operation_id`; control actions are typed `steer`, `resume`, and `interrupt`. Typed `resume` reconciles the retained thread and turn; it does not start a new turn or revive a terminated child process. Temote persists an accepted receipt before starting or controlling the child turn; an uncertain crash returns `reconciliation_required` instead of replaying a side effect. In `ask` mode these operations require local approval; `agent` and `yolo` sessions skip the Temote-local prompt. Approval details identify Codex provenance, operation/tool, target and scope, mutation/read-only status, and safe model/effort or command/file-change summaries. Prompts, control input, transcripts, raw command arguments, patch bodies, and command output are not placed in task metadata or approval/activity summaries. `codex_task_get` exposes only bounded, expiring, session-and-scope-bound evidence through an opaque `evidence_id`.

Temote yolo changes only Temote's own local sandbox and approval behavior; it does not authorize Codex child mutations. Codex app-server command and file-change approval requests keep the child approval boundary and fail closed when the user-approval transport is unavailable. Pre-thread initialization or model-list failures are reported as `retryable_failed` and the same start operation can be retried; after a thread/start or turn/start request may have been sent, replay remains `reconciliation_required`. `codex_task_get` reconciles the remote thread before applying `after_revision`/`not_modified`. When another Temote process owns the live app-server runtime, it returns the persisted revision with `reconciliation_deferred: true` and does not start a competing resume; typed control fails before its operation receipt is persisted. While that lease remains live, session cleanup does not overwrite the task; the runtime owner shuts down and finalizes it after observing session termination. Task records are retained for the full task-retention period, including unexpired terminal records; only expired terminal records without a live child runtime may be pruned. A scope at its retention limit rejects a new start instead of deleting an unexpired record, and compacted operation receipts retain exact-replay conflict protection during retention.

Generated turns are requested with Codex `workspaceWrite`, the session's canonical directory as the writable root, and network access disabled. This is an experimental app-server adapter, not the same OS-level boundary as Temote's session sandbox: the app-server process itself communicates with the inference service outside that boundary. It exposes no generic JSON-RPC, remote shell, or automatic approval path. If the installed Codex build or its sandbox behavior cannot be validated, keep these surfaces disabled/opt-in.

### Experimental OpenCode tasks

The opt-in `opencode_status`, `opencode_task_start`, `opencode_task_get`, and `opencode_task_control` tools spawn a per-task `opencode serve` child on loopback and talk to it through the `unofficial-opencode-sdk` HTTP client. Each serve child runs on 127.0.0.1 with a dynamically assigned port, a per-instance random Basic-auth password passed only through the child environment, an isolated per-task data directory, and a bounded serve permission configuration injected through `OPENCODE_CONFIG_CONTENT`. It inherits the host OpenCode global configuration (including provider/model definitions). Temote seeds its private task state with legacy `auth.json` and, for OpenCode V2, credentials from the host SQLite database while excluding host sessions and history. Connect the provider in the host CLI with `opencode auth login` first; the child uses credentials as of spawn time, and its token refreshes do not update the host account. The host database must have the supported V2 credential schema; an unsupported database fails closed. Task records, ownership, leases, receipts, retention, and scoped evidence follow the same contract as the Codex app-server tasks above: a task is owned by the complete session instance and its canonical working directory and cannot be resumed from another session, process generation, or scope.

`opencode_task_start` and `opencode_task_control` require an opaque `operation_id`; control actions are typed `steer`, `resume`, and `interrupt`. Steer sends another prompt on the retained session; resume reconciles the retained session and respawns a fresh serve child against the same per-task state directory — it does not start a new task or revive a terminated child. Prompts carry a deterministic `messageID` derived from the operation, so `opencode_task_get` can tell whether a start prompt was ever admitted before declaring `reconciliation_required`. Terminal-state reports are extracted from the last assistant message under the same bounded report contract as the other delegation backends; usage and the observed model are read from session messages rather than self-reported values. In `ask` mode they require local approval with the same provenance/scope/mutation metadata as Codex tasks, while `agent` and `yolo` sessions skip the Temote-local prompt; `opencode_task_get` exposes only bounded, expiring, session-and-scope-bound evidence.

This is an experimental serve adapter, not the OS-level boundary of Temote's session sandbox: the serve process itself communicates with provider APIs outside that boundary. Pending OpenCode permission/question requests surface as `waiting_approval` task state rather than auto-approval. Keep these surfaces opt-in until the host's installed OpenCode build has been validated end-to-end.

OpenCode 1.x and 2.x expose different HTTP contracts (`global/*` vs `api/*`). At spawn time Temote probes `global/health` first on every poll and tries the `api/*` liveness routes (`api/info`, then `api/health`) once the 1.x probe has failed, keeping whichever contract answers; both adapters expose the same task interface, so a build serving both resolves correctly either way. Set `TEMOTE_OPENCODE_SERVE_CONTRACT` to `v1` or `v2` to skip auto-detection and pin a contract; any other value (or the variable being unset) keeps auto-detection.

### Experimental Devin tasks

The opt-in `devin_status`, `devin_task_start`, `devin_task_get`, and `devin_task_control` tools spawn a per-task `devin acp` child and drive it over stdio JSON-RPC using the Agent Client Protocol (ACP). The child runs in the task's canonical scope directory with a filtered environment (PATH/HOME/proxy and the Devin/Windsurf credential variables only). Task records, ownership, leases, receipts, retention, and scoped evidence follow the same contract as the Codex and OpenCode tasks above: a task is owned by the complete session instance and its canonical working directory and cannot be resumed from another session, process generation, or scope.

`devin_task_start` and `devin_task_control` require an opaque `operation_id`; control actions are typed `steer`, `resume`, and `interrupt`. Steer sends another `session/prompt` on the retained ACP session; resume reattaches through `session/load` only when the agent advertises the `loadSession` capability — otherwise it fails closed without replaying; interrupt sends `session/cancel`. Each `session/prompt` call is a blocking ACP request whose `stopReason` result maps onto task status (`end_turn` completes, `cancelled` interrupts, other reasons become `retryable_failed`), so a turn that may have been sent but whose response was lost reconciles through the retained session state rather than replaying. Agent `session/request_permission` calls route through the Temote-local approval console as `waiting_approval` task state, never auto-approval. Terminal reports are extracted from the accumulated assistant message under the same bounded report contract as the other delegation backends. In `ask` mode these operations require local approval with the same Devin provenance/scope/mutation metadata, while `agent` and `yolo` sessions skip the Temote-local prompt; `devin_task_get` exposes only bounded, expiring, session-and-scope-bound evidence.

This is an experimental stdio adapter, not the OS-level boundary of Temote's session sandbox: the ACP child communicates with the Devin service outside that boundary, and authentication comes from the installed CLI's own credential state (`devin auth login`, `DEVIN_API_KEY`, or `WINDSURF_API_KEY`). Keep these surfaces opt-in until the host's installed Devin CLI has been validated end-to-end.

`devin_task_start` also accepts `cloud: true`, which spawns `devin acp --cloud` instead: the CLI relays the stdio ACP transport to the Devin Cloud ACP WebSocket, so the hosted session runs on Devin Cloud under the CLI's `auth login` account rather than as the local agent. `model` and `agent` are ignored by `devin acp --cloud` and are rejected when combined with `cloud`. The transport, ownership, and evidence contract is otherwise identical to the local mode.

### Experimental Devin Cloud tasks

The `devin acp` tools above drive a *local* Devin CLI. The separate `devin_cloud_status`, `devin_cloud_task_start`, `devin_cloud_task_get`, and `devin_cloud_task_control` tools drive *hosted* Devin sessions through the Devin API v3 (`https://api.devin.ai/v3/organizations/{org_id}/sessions`) over HTTPS from the Temote host; no Devin binary is required, and the session's work happens on Devin Cloud rather than in the Temote session's working directory. These tools exist only in `network`-feature builds.

Configure the credential with `TEMOTE_MCP_DEVIN_API_KEY` (a Devin service-user key or personal API key; `DEVIN_API_KEY` is accepted as a fallback), optionally pin the organization with `TEMOTE_MCP_DEVIN_ORG_ID` (otherwise it is resolved once from `/v3/self` and must be unambiguous), and optionally override the API origin with an `https://` `TEMOTE_MCP_DEVIN_API_BASE_URL`. Service users and personal access tokens are both created in the Devin organization settings of a normal (Teams/self-serve) subscription; sessions consume that subscription's ACUs. With a service-user key, set `TEMOTE_MCP_DEVIN_CREATE_AS_USER_ID` to your Devin user ID so sessions are attributed to you (`create_as_user_id`, requires the service user role to allow it) instead of to the service user. `devin_cloud_status` and `temote-mcp doctor` report the credential *source* (variable name), organization, and base URL only; the key value never appears in task records, approvals, evidence, or tool output.

`devin_cloud_task_start` requires an opaque `operation_id` and a `task`; `title`, `devin_mode`, `repos`, and `max_acu_limit` are optional and forwarded to session creation. Temote persists acceptance before calling the API, then creates one resumable session tagged `temote-mcp` whose prompt asks for the same bounded JSON report contract as the other delegation backends (also requested as structured output). Task records are owned by the complete session instance and its canonical working directory like Codex/OpenCode/Devin ACP tasks, live under `devin-cloud-tasks`, and are invisible to other sessions. `devin_cloud_task_get` reconciles the retained task against the hosted session status (`running`/`claimed` → `running`, `waiting_for_user` → `waiting_input` unless a terminal report was already published — Devin idles rather than exiting — in which case `completed`/`failed`, `waiting_for_approval` → `waiting_approval`, `suspended` by inactivity → `waiting_input`, `exit` → `completed`, `error`/quota/payment failures → `failed`, user termination → `interrupted`) and extracts the report from structured output first, then from the final Devin messages; final messages are exposed only through bounded scoped evidence. Control actions are `steer` (post a follow-up message), `resume` (message a suspended session, which Devin resumes), and `interrupt` (terminate the hosted session). Definite API rejections become `retryable_failed`; transport failures whose remote effect is uncertain become `reconciliation_required` and are never replayed blindly. In `ask` mode these operations require local approval with Devin Cloud provenance metadata (`scope: devin_cloud`) so the operator can see that the mutation consumes ACUs in the hosted organization rather than touching the local workspace.

### Delegation backend (local CLI)

`temote-mcp delegate --backend codex|opencode ...` runs one bounded, non-interactive delegation process from the local CLI and prints one bounded JSON result. OpenCode also accepts an explicit `--session <id>` for resume. Resume first performs a bounded, read-only `opencode session list --format json` preflight and requires the session's canonical directory to equal the current canonical delegation directory; missing, ambiguous, malformed, oversized, failed, or mismatched metadata fails closed before `run` starts. Adding `--fork` requires `--session` and starts a new session that inherits the named session's context; the same fail-closed directory preflight runs against the parent session before `run` starts. `--continue` and `--attach` are not supported. The OpenCode backend resolves its executable in this order:

1. `TEMOTE_OPENCODE_BIN`, when set, must be an absolute path to an existing executable regular file. Symlinks are resolved to their canonical target. It takes precedence over PATH.
2. otherwise `opencode` is resolved from PATH.

An explicitly configured but invalid `TEMOTE_OPENCODE_BIN` (empty, relative, missing, not a regular file, or not executable) fails closed; Temote does not silently fall back to a different PATH executable. The configured path is never printed in diagnostics or errors. `temote-mcp delegate diagnose --backend opencode` reports only `available`/`unavailable`, the source (`env_override`, `path`, or `invalid_override`), and a bounded reason for an invalid override. `TEMOTE_OPENCODE_BIN` is read by the parent process and is not passed to the OpenCode child environment.

## Git inside delegated sessions

Git metadata writes and remote synchronization run inside the delegated agent, not through Temote tools. When the delegated agent operates on a GitHub HTTPS remote, the repository-local managed credential mapping must be configured once per clone on the host:

```sh
git config --local credential.helper ''
git config --local --add credential.helper '!gh git credential --managed'
git config --local credential.useHttpPath true
```

As an opt-in alternative, the host-approved `ambient_git_credentials` session grant lets the same managed Git operations fall back to ambient Git credentials when no mapping exists; non-GitHub and SSH remotes keep their normal credential path in every case. Global `gh` auth state is never mutated.

## Yolo mode

```sh
temote-mcp start my-project --yolo
```

Yolo mode intentionally removes Temote MCP's path restrictions, command sandbox, and local approval prompts. Delegated child work runs with the filesystem, environment, process, and network permissions of the user running Temote MCP; Yolo does not disable authorization or confirmation imposed by an MCP client, a delegation backend, or another external system.

The detached supervisor does not automatically promote a running normal session to yolo mode. Start yolo explicitly through the local-only compatibility command when that trust level is intended.

## Local stdio

For MCP clients that launch Temote MCP directly:

```sh
temote-mcp mcp
```

## Safety notes

- Do not permit broad roots such as an entire home directory when a narrower project path is sufficient.
- There is no secret-file denylist; permitted roots are the primary filesystem boundary for session scope.
- Runtime audit records operation/status/timing metadata, not task bodies, child output, authenticated identity fields, or secret values.
- Delegated backends keep credentials inside the child/session process rather than session metadata.

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
