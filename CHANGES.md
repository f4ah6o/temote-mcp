# Changes

## Unreleased

### Added

- `opencode_task_*` serve children now use the host CLI's provider/model configuration and seed V2 saved credentials into private task state without sharing host sessions or history; legacy `auth.json` remains supported. ([OpenCode serve shared provider](issues/done/20260924-opencode-serve-shared-provider.md))
- The local-agent Git shim now supports merged-only `git branch -d <branch>` and lease-protected `git push <remote> --delete <branch>`. Remote deletion requires a fetch-established remote-tracking ref, preserves the structured default/protection checks, and rejects force, wildcard, arbitrary URL, and arbitrary refspec forms. ([safe Git shim cleanup commands](issues/done/20260916-git-shim-cleanup-commands.md))
- `local_agent_run` accepts `worktree: {branch, task?}` to bind a run to the selected repository's Temote-managed worktree without any caller-supplied path: Temote derives `<configured src root>/worktrees/<repo>/<task>` from the selected session workspace, reuses only a verified managed worktree of that repository on that branch, otherwise creates one through the approved path, rejects `cwd` combined with `worktree`, and re-validates the workspace immediately before the agent starts. `session_list`/`session_info` also report a bounded non-secret `workspace` identity derived from the session working directory. ([managed worktree session integration](issues/done/20260916-managed-worktree-session-integration.md))
- Added `git_worktree_create` and `git_worktree_list` for deterministic Temote-managed worktrees below the configured `src` named root (`~/src/worktrees/<repo>/<task>` in the usual layout), with read-only pre-approval inspection, exact `src` named-root authority, existing-local-branch-only creates, post-create containment/identity verification, fail-closed path/traversal/symlink/collision validation, and legacy worktree preservation. ([managed worktree create/list](issues/done/20260916-managed-worktree-create-list.md))
- Added `temote-mcp activity` for bounded local replay and live observation of session operations, lifecycle changes, approvals, jobs, integrations, and supervisor upgrades. Activity stays on the owner-only local control socket and is not a durable audit log. ([local activity viewer](issues/polished/20260914-local-activity-viewer.md))
- Added authenticated direct HTTP session lifecycle and supervisor upgrade tools with explicit local approval, durable transaction status, and reconnect-after-commit behavior. ([client-safe upgrade reconnect](issues/open/20260908-07-client-safe-upgrade-reconnect.md))
- Added opt-in structured Codex task tools backed by a version-checked local app server, typed control actions, bounded evidence, and persistent reconciliation receipts. ([Codex delegation and app server](issues/open/20260908-08-codex-delegation-dogfood-and-app-server.md))
- Added opt-in `opencode_status`, `opencode_task_start`, `opencode_task_get`, and `opencode_task_control` tools that drive a per-task `opencode serve` child over the `unofficial-opencode-sdk` client with the same ownership, lease, receipt, retention, and scoped-evidence contract as the Codex task tools. Each serve child runs on loopback with a dynamic port, an instance-scoped Basic-auth password, an isolated data directory, and a bounded permission configuration. ([server-primary agent backends](issues/open/20260922-agent-server-backends-cli-deprecation.md))

### Changed

- Supervisor upgrades now coordinate session restore, direct-ingress recovery, endpoint checks, and binary-owned Codex plugin reconciliation before reporting terminal status.
- `agent` permission mode now runs ordinary `execute`/`start_command` in the same sandbox and path containment with the network-enabled development profile, so localhost, LAN, and Internet development traffic works without yolo. `ask` keeps ordinary commands network-disabled, and public HTTP still cannot expose `without_sandbox` or create/promote `yolo`. ([agent development network access](issues/open/20260915-agent-development-network-access.md))

### Fixed

- Upgrading from an older, protocol-compatible supervisor now checks the installed Linux sandbox helper locally when that supervisor does not report its helper generation, so a compatible bundle is not incorrectly blocked as `unavailable`. ([legacy upgrade helper preflight](issues/open/20260924-upgrade-legacy-helper-preflight.md))
- Linked-worktree Git broker mutations now succeed in the default `agent` mode: the broker pins the selected worktree's validated repository identity and the sandbox authorizes only that repository's own Git metadata, while swapped or symlinked metadata still fails closed. Missing protected metadata masks (for example `packed-refs`) also stay readable inside the Linux sandbox instead of failing with `EACCES`. ([linked worktree broker metadata scope](issues/done/20260917-linked-worktree-broker-metadata-scope.md))
- `session_list` no longer fails when a supervisor-owned session's working directory is gone, and `session_info` reports the same bounded `degraded` view while leaving the stale metadata untouched. ([session list missing cwd](issues/done/20260917-session-list-supervisor-owned-missing-cwd.md))
- Codex task cleanup and recovery now preserve accepted operations through process and session transitions without replaying uncertain side effects.

### Deprecated

### Removed

### Security

- Bound activity producers and Codex task runtimes to the exact private session instance so delayed work cannot be attributed to a same-name replacement session.
- Kept activity summaries and durable upgrade/task state bounded and free of command output, prompts, session paths, credentials, and raw errors.

### Migration
