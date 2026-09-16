# Structured Git cleanup slice 2: lease-protected local/remote branch delete

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-repository-triage-end-to-end.md`
Depends on: `issues/polished/20260916-structured-worktree-remove.md`

## Goal

Provide bounded branch cleanup operations needed to converge a reviewed repository to `main` only without exposing arbitrary refspec/force deletion.

## Scope

Local delete:
- exact validated unqualified local branch only;
- never current branch or branch checked out in any worktree;
- default merged-only semantics; no force-delete input.

Remote delete:
- exact validated unqualified branch on configured safe remote only;
- caller supplies expected remote SHA obtained during review;
- Temote constructs the exact delete ref internally and fails if the remote tip drifted;
- no arbitrary URL/refspec, wildcard, tag deletion, or unconditional force.

Add generated/PBT ref validation and local bare-remote lease/drift tests.

## Acceptance

Merged local deletion works; unmerged/current/worktree-bound deletion fails closed; remote deletion succeeds only at expected SHA and rejects drift; existing push safety remains green; `just sandboxed-check` passes.
