# Local-agent Git shim slice 5: safe worktree/branch cleanup syntax

Status: done
Model: GPT-5.6 Sol
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

## 2026-09-22 implementation evidence

- Added exact `branch -d <branch>` and `push <remote> --delete <branch>` classifier forms; force, glob, refspec, URL, injection, ordering, and argument escapes remain rejected.
- Remote shim deletion binds its lease only from the exact pinned `refs/remotes/<remote>/<branch>` object ID; missing tracking context fails before approval/network mutation, and stale tracking state preserves the concurrently updated remote branch.
- Deterministic broker coverage proves merged local deletion, nested-cwd lease-protected remote deletion, unrelated dirty/untracked work and refs preservation, missing tracking rejection, and stale-lease rejection. Existing structured remote path-swap coverage remains green.
- Focused checks: `cargo test --bin temote-mcp --all-features --locked agent_git` (42 passed, 1 ignored) and `cargo test --bin temote-mcp --all-features --locked mcp::tests::structured_git_remote_branch_delete_stays_bound_after_cwd_path_swap -- --exact` (passed).
- Every constituent `just sandboxed-check` command passed independently: all-feature library and targeted binary tests, no-default all-targets/check, clippy, gateway sandbox tests, format, and diff-check. The aggregate wrapper itself is **NOT RUN to completion**: three attempts were cancelled when the live OpenCode server restarted, so this record does not label the wrapper PASS.
- `CHANGES.md` records the user-visible ordinary-Git cleanup forms. Host-only nested sandbox and process-boundary acceptance remains in the repository's normal host/CI gate rather than being relabeled PASS here.
