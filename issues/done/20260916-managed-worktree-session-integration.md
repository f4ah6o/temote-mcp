# Managed worktree broker slice 2: session and local-agent integration

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-temote-managed-worktree-broker.md`
Depends on: `20260916-managed-worktree-create-list.md`

## Goal

Make Temote, not the coding agent, choose/reuse the managed worktree and bind it to the session/project root before `local_agent_run` starts.

## Scope

- represent canonical repository root, managed workspace root, branch and workspace type in bounded non-secret session/workspace metadata;
- for a task requiring an isolated worktree, reuse the matching managed worktree or create one through the broker before agent startup;
- set the coding-agent cwd/project root to the selected managed worktree;
- validate the workspace immediately before agent launch so prompt compliance is not the enforcement mechanism;
- reject new-task automatic use of legacy worktrees outside the managed root, while preserving them unchanged;
- keep canonical checkout use valid for tasks that explicitly do not require an isolated worktree.

Do not implement worktree removal/prune here.

## Acceptance

- local_agent_run starts inside the selected managed worktree without the model choosing a path;
- session/workspace identity survives normal session inspection without secret/path ambiguity;
- managed-root escape and wrong-repository reuse fail closed;
- legacy worktrees are discovered but not automatically adopted/moved/deleted;
- existing session/path containment and protected metadata tests remain green;
- focused tests and `just sandboxed-check` pass.

## Implementation notes (2026-09-17)

`src/managed_worktree.rs`:

- `SessionWorkspaceType` / `SessionWorkspace` and `inspect_session_workspace(cwd, src_root)`
  derive a bounded non-secret workspace identity from the canonical session working directory.
  Managed classification requires the exact trusted managed root, canonical direct-child
  containment and a matching repository identity; symlinked or swapped roots, the namespace
  directory itself, `<repo>/.wt/*`, `<src>/<repo>-*` and `/tmp` worktrees resolve to
  `legacy_worktree` and are never adopted.
- `verify_reusable_managed_worktree` requires a canonical direct child of the trusted managed
  root with the selected repository's common Git directory, primary checkout and requested
  branch. Wrong-repository, wrong-branch, symlinked and legacy targets fail closed.
- `configured_src_root_from_env` centralizes the `TEMOTE_MCP_ROOTS` `src` named-root lookup.

`src/sandbox.rs`:

- `git_current_branch` reads the worktree's own bounded `HEAD` control file (detached HEAD
  yields `None`), so a linked worktree reports its own branch.

`src/mcp.rs`:

- `local_agent_run` accepts `worktree: {branch, task?}` and rejects `cwd` combined with it.
  Temote resolves the canonical repository from the selected session workspace, verifies the
  session already permits that repository's checkout, derives the managed target, reuses only a
  `verify_reusable_managed_worktree` target, and otherwise creates through the existing
  approved `git_worktree_create` path.
- The binding adds exactly the validated managed worktree root to that run's derived sandbox
  session; the on-disk session keeps its own permitted roots and the primary checkout/legacy
  worktrees are untouched.
- The workspace is re-derived and re-verified immediately before `spawn_local_agent`, and the
  on-disk session's cwd/permitted roots are re-checked after approval.
- `git_worktree_create` shares the extracted `create_managed_worktree` path, so create/list
  behavior and result contracts are unchanged.

`src/session_control.rs`:

- `SessionView` gains an optional `workspace` field derived from the session cwd; degraded or
  unresolvable workspaces report none instead of stale identity.

`src/local_agent.rs`:

- `PreparedRun` carries the derived workspace identity into the approval detail/metadata
  (`workspace_type`, `repository_root`, `workspace_root`, `repository`, `branch`, `task`), all
  bounded and non-secret.

Gateway contract and docs: `local_agent_run` schema/snapshot/fingerprint updated with the
bounded `worktree` object; `docs/usage.md` / `docs/usage.ja.md` and the Agent Skill describe the
binding and the session workspace view.

Focused tests (repository-local):

- `managed_worktree::tests::session_workspace_identity_uses_the_exact_managed_authority`
- `managed_worktree::tests::reusable_managed_worktree_requires_identity_and_branch`
- `sandbox::generic_tests::current_branch_reads_the_worktree_own_head`
- `mcp::tests::local_agent_worktree_input_is_bounded_and_path_free`
- `mcp::tests::local_agent_worktree_binding_reuses_and_creates_verified_managed_worktrees`
- `mcp::tests::local_agent_worktree_binding_rejects_path_injection_and_wrong_reuse`
- `mcp::tests::local_agent_worktree_binding_denied_approval_creates_nothing`
- `session_control::tests::session_view_reports_the_derived_workspace_identity`
- existing managed-worktree, local-agent and session-view suites

Results in this round: `just sandboxed-check` PASS; gateway sandbox suite PASS. Host gates
(`just linux-sandbox-acceptance`) stay as recorded in the Goal B round. Remove/prune, branch
delete, automatic cleanup and legacy migration stay out of scope for this packet.
