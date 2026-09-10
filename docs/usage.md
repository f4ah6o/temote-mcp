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

`session list` includes durable `starting`, `active`, `stopping`, `stopped`, and `crashed` states. `session info` includes the working directory, permitted roots, permission mode, timestamps, exit reason, and last error. A dead or ambiguous socket is never silently treated as active. Manual restart is available with `temote-mcp session restart <id>`; automatic restart is not enabled. Restart fences the old full session instance, shuts down its registered Codex runtimes before starting the replacement, and does not leave those child runtimes running if replacement startup fails.

Session discovery is active-first: sessions owned by the running supervisor are returned before bounded historical metadata, so accumulated history cannot evict active sessions from `session list` / MCP `session_list`. Historical stopped/crashed entries are returned in a deterministic recent-first order within the list budget. Supervisor startup and periodic maintenance retain the 512 most recent safely confirmed terminal metadata pairs and prune only older confirmed stopped/crashed pairs. Live, ambiguous, malformed/orphaned, and supervisor-upgrade restore-plan-protected metadata is never automatically removed by retention; read-only listing and MCP fallback do not perform cleanup.

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

`session_stop` can stop only sessions marked as HTTP-owned by the lifecycle supervisor; it cannot stop local CLI/yolo sessions even though they share the same supervisor process. HTTP managed sessions are always non-yolo and retain the same local approval gates through `temote-mcp session console`. Public session-bound tools also reject separately started yolo sessions, so remote access cannot inherit their unrestricted local semantics. Stopped/crashed metadata remains visible through `session_list` / `session_info`; ordinary session-bound tools still require an active socket. `temote-mcp down` stops only the HTTP origin and its managed ingress child, not the lifecycle supervisor or its sessions. In a repository checkout, `just up/down` are development wrappers around these installed-binary commands.

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

`job_list({session_id, limit?})` returns a redacted snapshot of the current session's in-memory jobs. It reports only `job_id` and `running` / `completed` / `failed` / `unknown`, with running jobs first and a `truncated` flag. It never returns command text, argv, stdout/stderr, or raw errors, and listing does not consume a completed result. `retention="in_memory"` is explicit: an empty list is not proof that no work ran before restart or cache expiry.

The combined stdout/stderr retained for a command is capped at 1 MiB and reports when output was truncated.

### Experimental Codex tasks

The opt-in `codex_status`, `codex_task_start`, `codex_task_get`, and `codex_task_control` tools connect to a local `codex app-server --stdio` and accept only the named status/task operations. The app-server handshake is version-checked (`0.153.4`). A task is owned by the complete session instance and its canonical working directory, so it cannot be resumed from another session, process generation, or scope.

`codex_task_start` and `codex_task_control` require an opaque `operation_id`; control actions are typed `steer`, `resume`, and `interrupt`. Temote persists an accepted receipt before starting or controlling the child turn; an uncertain crash returns `reconciliation_required` instead of replaying a side effect. Normal sessions require local approval. Approval details identify Codex provenance, operation/tool, target and scope, mutation/read-only status, and safe model/effort or command/file-change summaries. Prompts, control input, transcripts, raw command arguments, patch bodies, and command output are not placed in task metadata or approval/activity summaries. `codex_task_get` exposes only bounded, expiring, session-and-scope-bound evidence through an opaque `evidence_id`.

Temote yolo changes only Temote's own local sandbox and approval behavior; it does not authorize Codex child mutations. Codex app-server command and file-change approval requests keep the child approval boundary and fail closed when the user-approval transport is unavailable. Pre-thread initialization or model-list failures are reported as `retryable_failed` and the same start operation can be retried; after a thread/start or turn/start request may have been sent, replay remains `reconciliation_required`. `codex_task_get` reconciles the remote thread before applying `after_revision`/`not_modified`. Task records are retained for the full task-retention period, including unexpired terminal records; only expired terminal records without a live child runtime may be pruned. A scope at its retention limit rejects a new start instead of deleting an unexpired record, and compacted operation receipts retain exact-replay conflict protection during retention.

Generated turns are requested with Codex `workspaceWrite`, the session's canonical directory as the writable root, and network access disabled. This is an experimental app-server adapter, not the same OS-level boundary as Temote's direct `execute` sandbox: the app-server process itself communicates with the inference service outside that direct command sandbox. It exposes no generic JSON-RPC, remote shell, or automatic approval path. If the installed Codex build or its sandbox behavior cannot be validated, keep these surfaces disabled/opt-in.

### Structured local agent broker

`local_agent_run({session_id, agent, task, cwd?, access, model?, profile?})` runs one locally installed Codex or OpenCode agent through a structured broker. `agent` is limited to `codex` and `opencode`; the caller supplies a bounded task and an access mode, not an executable, raw argv, environment, or network policy. Temote constructs the adapter-specific command line and verifies the installed non-interactive CLI contracts before shipping this feature.

The optional `cwd` is canonicalized and must remain inside a permitted session root, including after symlink resolution, even when the session is yolo. The `read_only` and `workspace_write` modes select the corresponding Codex sandbox and OpenCode edit policy. Temote's outer agent profile makes only the selected workspace roots writable for `workspace_write`; `read_only` leaves them read-only, while private per-run state/cache remains writable in either mode. `.git`, `.agents`, and `.codex` remain protected.

Every local-agent request crosses the local approval boundary, including from a yolo session. A denial is returned before the child process is started. Tasks are limited to 1 MiB, combined child output is limited to 1 MiB, and work longer than the foreground timeout returns a normal Temote `job_id` that can be inspected with `poll_job` or cancelled with `stop_job`. Approval and activity records contain only the agent, scope, access mode, task byte count, and a bounded task hash; they do not contain the task or environment values.

The child environment is cleared and rebuilt from a small allow-list, so Temote-held credentials, tokens, and proxy settings are not forwarded implicitly. The outer profile hides the host temporary and user-agent state roots, then re-exposes only the selected workspace, executable directories, and private run state needed for this invocation. Existing Codex (`~/.codex/auth.json`) and OpenCode (`~/.local/share/opencode/auth.json`) login files are imported as bounded read-only inputs into the private run state; the original user files are never writable by the child. Agent edits are not Git remote authorization; use the dedicated `git_*` tools for staging, commits, fetch, pull, and push. Public HTTP exposes only this structured broker and continues to omit the generic `without_sandbox` tool.

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
