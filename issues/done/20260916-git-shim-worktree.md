# Local-agent Git shim slice 3: map worktree syntax onto Temote-managed lifecycle

Status: done — existing implementation verified at `a3591d7`.
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: `20260916-managed-worktree-create-list.md` + `20260916-managed-worktree-session-integration.md` + `20260916-structured-worktree-remove.md`

## Goal

Let an agent use familiar worktree intent without choosing filesystem placement or receiving broad `.git/worktrees` write access.

## Scope

- expose/list the managed worktree inventory through the normal Git-developer UX;
- translate supported create intent into the managed broker so the destination is always `~/src/worktrees/<repo>/<task>`;
- translate supported remove intent into the safe structured remove operation;
- never honor an agent-supplied arbitrary destination path, even when expressed through ordinary `git worktree add <path>` syntax;
- return a bounded Temote policy error when requested syntax attempts to choose a path outside broker policy;
- preserve discovered legacy worktrees without adopting, moving or deleting them.

The broker owns worktree location and lifecycle; the shim is only a compatibility/UX layer.

## Acceptance

- list/create/remove works for one clean managed fixture worktree;
- arbitrary path selection is rejected before Git metadata mutation;
- branch names containing `/` still produce one broker-derived task directory;
- dirty/active/legacy/sibling worktrees are preserved according to broker rules;
- local-agent protected Git metadata boundaries remain intact;
- focused tests and `just sandboxed-check` pass.

## Completion review — 2026-09-22

`shim_worktree_forms_delegate_to_the_managed_broker` and `shim_worktree_remove_rejects_dirty_worktrees_and_missing_src_authority` pass with the managed lifecycle containment tests in Linux and macOS CI. The broker continues to own placement and protected metadata; no arbitrary path or legacy adoption was added. Constituent gate evidence is in `docs/evaluations/20260922-interrupted-opencode-recovery-review.md`. Later branch-delete/cleanup macOS failures remain separate, unresolved work.
