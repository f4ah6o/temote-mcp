# Local-agent Git shim slice 5: safe worktree/branch cleanup syntax

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: Git shim slices 1-4 + structured worktree remove + structured branch delete

## Goal

Let the local agent use the narrow ordinary Git cleanup syntax needed by repository triage while delegating every mutation to the validated structured cleanup operations.

## Supported forms

- `git worktree remove <known-clean-.wt-path>`
- `git branch -d <merged-local-branch>`
- `git push <configured-remote> --delete <branch>` only when the broker can bind the operation to the reviewed expected remote SHA; if that expected-tip context is unavailable, reject and require parent/coordinator cleanup rather than weakening the lease.

Reject `-D`, `--force`, wildcards, arbitrary refs/URLs, config/alias injection, and cleanup of active/dirty worktrees.

## Acceptance

A deterministic triage fixture can remove one reviewed obsolete worktree/local branch and lease-protected remote branch through ordinary Git syntax; unrelated dirty work and active refs remain byte/ref-identical; `just sandboxed-check` passes.
