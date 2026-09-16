# Managed worktree broker slice 3: safe structured worktree remove

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-temote-managed-worktree-broker.md`
Depends on: `20260916-managed-worktree-create-list.md` + session/job ownership evidence

## Goal

Remove only a known Temote-managed linked worktree after proving it is safe, without touching legacy worktrees or unrelated worker state.

## Scope

- target must be a registered managed worktree under `~/src/worktrees/<repo>/<task>` derived by broker policy; caller does not supply an arbitrary path;
- refuse canonical primary checkout, legacy worktrees outside the managed root, unknown path, symlink escape, wrong repository, current session cwd, or worktree owned by another active session/running job;
- refuse dirty/untracked work; never auto-stash/reset/clean/force;
- remove only the selected clean worktree and its own Git worktree metadata;
- add deterministic/PBT path tests plus dirty, active-session, sibling and legacy-preservation tests.

Do not delete branches, remote refs, or real directories unrelated to the selected registered worktree.

## Acceptance

- clean selected managed worktree removal succeeds;
- dirty/untracked/active/wrong-repo/legacy/sibling cases fail closed or are preserved as appropriate;
- existing canonical checkout and unrelated worktrees are unchanged;
- public/gateway contract snapshots are synchronized if this operation is public;
- focused tests and `just sandboxed-check` pass.
