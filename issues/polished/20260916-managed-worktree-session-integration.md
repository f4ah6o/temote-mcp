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
