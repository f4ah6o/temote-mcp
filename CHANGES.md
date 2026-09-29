# Changes

## Unreleased

### Added

- `temote-mcp upgrade --dry-run` now lists each unrestorable session's `session_id` and reason in `blocked_sessions`, and `temote-mcp upgrade --force` stops those sessions before the handoff (they are not restored) instead of aborting; protocol, ingress, and helper compatibility gates still apply. ([upgrade force and blocked session list](issues/open/20260928-upgrade-force-blocked-sessions.md))
- Fabric now synchronizes eligible host observations automatically and resolves bounded repository context with provenance; a configured Memory Worker can derive supported knowledge from the D1 replica. Instruction and error previews remain opt-in, and their policy must stay unchanged until pending batches are acknowledged. ([cloud observation and knowledge plane](issues/open/20260926-cloud-observation-knowledge-plane.md))
- Added `repository_clone_bare`, an idempotent public tool that admits an active non-yolo session at an exact supervisor-configured named root, accepts same-root local or credential-free HTTPS sources, rejects existing or escaping destinations, and delegates an atomically claimed bare Git clone through the retained Codex task lifecycle without exposing an absolute host path.
- `temote-mcp doctor` checks Jujutsu (`jj --version`) for development readiness and warns when jj is missing or unusable without requiring it for normal operation.
- Added a repository-owned dogfood harness for versioned logical scenarios, bounded call observations, deterministic fault fixtures, live MCP runs, and evidence-linked before/after comparison. ([self-improvement protocol](issues/done/20260928-self-improvement-dogfood-protocol.md))
- `task_list` rediscovers session-owned delegated tasks across backends through the existing bounded task projection, with per-backend availability and no transcript or output. ([development harness](issues/open/20260924-temote-development-harness-restructure.md))
- `opencode_task_*` serve children now use the host CLI's provider/model configuration and seed V2 saved credentials into private task state without sharing host sessions or history; legacy `auth.json` remains supported. ([OpenCode serve shared provider](issues/done/20260924-opencode-serve-shared-provider.md))
- `session_list`/`session_info` report a bounded non-secret `workspace` identity derived from the session working directory (`workspace_type` of `canonical_checkout` / `managed_worktree` / `legacy_worktree`, plus repository and branch when resolvable). ([managed worktree session integration](issues/done/20260916-managed-worktree-session-integration.md))
- Added `git_worktree_create` and `git_worktree_list` for deterministic Temote-managed worktrees below the configured `src` named root (`~/src/worktrees/<repo>/<task>` in the usual layout), with read-only pre-approval inspection, exact `src` named-root authority, existing-local-branch-only creates, post-create containment/identity verification, fail-closed path/traversal/symlink/collision validation, and legacy worktree preservation. ([managed worktree create/list](issues/done/20260916-managed-worktree-create-list.md))
- Added `temote-mcp activity` for bounded local replay and live observation of session operations, lifecycle changes, approvals, jobs, integrations, and supervisor upgrades. Activity stays on the owner-only local control socket and is not a durable audit log. ([local activity viewer](issues/polished/20260914-local-activity-viewer.md))
- Added authenticated direct HTTP session lifecycle and supervisor upgrade tools with explicit local approval, durable transaction status, and reconnect-after-commit behavior. ([client-safe upgrade reconnect](issues/open/20260908-07-client-safe-upgrade-reconnect.md))
- Added opt-in structured Codex task tools backed by a version-checked local app server, typed control actions, bounded evidence, and persistent reconciliation receipts. ([Codex delegation and app server](issues/open/20260908-08-codex-delegation-dogfood-and-app-server.md))
- Added opt-in `opencode_status`, `opencode_task_start`, `opencode_task_get`, and `opencode_task_control` tools that drive a per-task `opencode serve` child over the `unofficial-opencode-sdk` client with the same ownership, lease, receipt, retention, and scoped-evidence contract as the Codex task tools. Each serve child runs on loopback with a dynamic port, an instance-scoped Basic-auth password, an isolated data directory, and a bounded permission configuration. ([server-primary agent backends](issues/open/20260922-agent-server-backends-cli-deprecation.md))

### Changed

- Dogfood comparisons accept completed, verified issue work without requiring a measured Temote improvement; reports distinguish `improved`, `unchanged`, and `regressed`, while regressions and missing acceptance evidence remain blocked.
- Supervisor upgrades now coordinate session restore, direct-ingress recovery, endpoint checks, and binary-owned Codex plugin reconciliation before reporting terminal status.
- `agent` permission mode now runs ordinary `execute`/`start_command` in the same sandbox and path containment with the network-enabled development profile, so localhost, LAN, and Internet development traffic works without yolo. `ask` keeps ordinary commands network-disabled, and public HTTP still cannot expose `without_sandbox` or create/promote `yolo`. ([agent development network access](issues/open/20260915-agent-development-network-access.md))

### Fixed

- Accepted Codex task runtimes now tolerate one-off session metadata or liveness probe failures while monitoring their already-authorized owner. Verified inactive or replaced sessions still stop immediately, and three consecutive unknown observations stop fail-closed.
- Session lifecycle admission now accepts standard linked worktrees backed by bare repositories, using the validated canonical common Git directory as the repository reservation identity. Managed-worktree authority and Git mutation brokers still require the supported primary-checkout layout, and malformed reciprocal pointers or symlinked `.git` metadata remain rejected.
- Completed OpenCode and Devin ACP tasks keep malformed final replies recoverable through scoped evidence. Task responses show report decode status and truncation without changing execution status. ([completed task result recovery](issues/open/20260927-completed-task-malformed-final-report-json.md))
- Local supervisor control requests no longer fail when macOS reports `ENOTCONN` while half-closing an already-written request; the client still requires the supervisor response and never replays the mutation. Session metadata E2E fixtures also wait for active-session and retention quiescence before parity/determinism assertions. ([CLI/MCP parity flake](issues/open/20260922-cli-mcp-parity-enotconn-flake.md), [retention macOS flakes](issues/open/20260923-retention-e2e-macos-race-flakes.md))
- Upgrading from an older, protocol-compatible supervisor now checks the installed Linux sandbox helper locally when that supervisor does not report its helper generation, so a compatible bundle is not incorrectly blocked as `unavailable`. ([legacy upgrade helper preflight](issues/open/20260924-upgrade-legacy-helper-preflight.md))
- Linked-worktree Git broker mutations now succeed in the default `agent` mode: the broker pins the selected worktree's validated repository identity and the sandbox authorizes only that repository's own Git metadata, while swapped or symlinked metadata still fails closed. Missing protected metadata masks (for example `packed-refs`) also stay readable inside the Linux sandbox instead of failing with `EACCES`. ([linked worktree broker metadata scope](issues/done/20260917-linked-worktree-broker-metadata-scope.md))
- `session_list` no longer fails when a supervisor-owned session's working directory is gone, and `session_info` reports the same bounded `degraded` view while leaving the stale metadata untouched. ([session list missing cwd](issues/done/20260917-session-list-supervisor-owned-missing-cwd.md))
- Codex task cleanup and recovery now preserve accepted operations through process and session transitions without replaying uncertain side effects.

### Deprecated

### Removed

- Removed `local_agent_run`, the per-command one-shot Codex/OpenCode broker, together with its private Git shim (`git-shim`), the dedicated local-agent sandbox profile, and the parent-side Git broker. Delegation is served only by the server-backed `codex_task_*` / `opencode_task_*` / `devin_*` tools; `temote-mcp delegate` / `temote-mcp codex delegate` remain the legacy one-shot CLI fallback. ([server-primary agent backends](issues/open/20260922-agent-server-backends-cli-deprecation.md))

### Security

- Bound activity producers and Codex task runtimes to the exact private session instance so delayed work cannot be attributed to a same-name replacement session.
- Kept activity summaries and durable upgrade/task state bounded and free of command output, prompts, session paths, credentials, and raw errors.

### Migration

- Existing Fabric D1 deployments must inspect and apply pending observation and memory migrations before deploying this Worker update. Migration `0003_memory_worker.sql` rebuilds knowledge tables while copying their existing rows and provenance. Memory extraction remains disabled by default; enable it through Worker variables and the `MEMORY_API_KEY` secret when ready. ([Fabric operations](docs/gateway.md#deploy))
