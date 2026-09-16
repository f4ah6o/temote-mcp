# Managed worktree broker slice 4: metadata-only prune

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-temote-managed-worktree-broker.md`
Depends on: `20260916-managed-worktree-create-list.md` + `20260916-structured-worktree-remove.md`

## Goal

Add a bounded prune operation that cleans only stale Git worktree metadata and never invents filesystem deletion semantics.

## Scope

- operate only on the resolved canonical repository;
- prune only Git metadata entries Git itself classifies as stale;
- never delete a filesystem directory merely because metadata is stale;
- never remove a live managed worktree, active-session workspace, dirty worktree or legacy worktree;
- expose bounded before/after counts or identities without arbitrary Git output passthrough;
- keep caller inputs path-free and fail closed on ambiguous repository identity.

Do not delete branches or remote refs in this packet.

## Acceptance

- stale metadata fixture is pruned while its unrelated filesystem fixtures remain unchanged;
- real managed/legacy worktree directories are preserved;
- active session/job ownership prevents conflicting cleanup;
- repeated prune is idempotent;
- focused tests and `just sandboxed-check` pass.
