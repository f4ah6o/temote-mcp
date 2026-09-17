# Managed worktree broker slice 1: deterministic create/list policy

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-temote-managed-worktree-broker.md`
Depends on: current structured Git repository identity primitives

## Goal

Create/list Temote-managed worktrees only under `~/src/worktrees/<repo>/<task>` without accepting arbitrary paths from callers.

## Scope

- resolve the canonical repository from the selected session/repository identity rather than trusting a basename alone;
- derive managed root `~/src/worktrees/<repo>` and a filesystem-safe task directory independent from branch `/` hierarchy;
- expose structured create/list operations with repository, branch and optional task name inputs only;
- reject absolute paths, `..`, option-like/path injection, symlink escape and collisions;
- preserve every existing legacy worktree outside the managed root; do not move/delete it;
- list managed and legacy worktrees with an explicit classification so later lifecycle code can distinguish them.

Do not integrate session creation/local_agent_run yet. Do not remove or prune worktrees in this packet.

## Acceptance

- managed create lands exactly below `~/src/worktrees/<repo>/<task>`;
- branch names containing `/` do not create nested task directories;
- traversal/absolute/symlink/collision generated tests fail closed;
- existing `.wt`, sibling, `/tmp`, or `~/src/<repo>-*` legacy worktrees are unchanged;
- public/gateway contract snapshots are synchronized if the operation is public;
- focused tests and `just sandboxed-check` pass.
