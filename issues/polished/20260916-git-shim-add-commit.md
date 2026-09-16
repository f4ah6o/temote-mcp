# Local-agent Git shim slice 2: add and commit

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: `20260916-git-shim-switch-create.md`

## Goal

Allow ordinary `git add <paths>` and `git commit -m <message>` inside local-agent development flow through bounded parent-side Git operations.

## Scope

- reuse existing `git_add`/`git_commit` safety semantics;
- support explicit relative paths inside current repository and one message form;
- protect unrelated dirty/untracked work from accidental staging;
- reject `-A`, `--all`, path escape, config/alias injection, hooks/signing override, and arbitrary commit plumbing in this slice.

Do not implement fetch/pull/push or worktrees here.

## Acceptance

- agent can edit one file, `git add` only that file, and commit it;
- unrelated modified/untracked files remain unstaged/unchanged;
- hooks/signing remain disabled as in structured Git tools;
- protected metadata is not broadly writable;
- focused/PBT path validation and `just sandboxed-check` pass.
