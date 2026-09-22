# Structured Git cleanup slice 2: lease-protected local/remote branch delete

Status: polished / blocker remediation implemented; independent review pending
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-repository-triage-end-to-end.md`
Depends on: `issues/done/20260916-structured-worktree-remove.md`

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
- the live remote symbolic `HEAD` is authoritative for the default branch and that branch is never deletable;
- GitHub destinations require live branch metadata with `protected=false`;
- non-GitHub destinations require a valid repository-local `temote.remote.<remote>.protectedBranch` policy;
- missing, invalid, ambiguous, or unavailable default/protection state fails closed;
- no arbitrary URL/refspec, wildcard, tag deletion, or unconditional force.

Add generated/PBT ref validation and local bare-remote lease/drift tests.

## Acceptance

Merged local deletion works; unmerged/current/worktree-bound deletion fails closed; local deletion remains safe if the caller cwd is path-swapped while approval is pending; remote deletion succeeds only at expected SHA and rejects drift; default/protected/missing/invalid remote state fails closed; existing push safety remains green; `just sandboxed-check` passes.

## 2026-09-22 blocker remediation

- Local deletion pins the worktree, private Git metadata, and common Git metadata by descriptor before approval, then runs the exact merged-only delete through those descriptors and fixed Git environment. A cwd path swap cannot redirect the mutation to another repository.
- Remote deletion pins the worktree, private Git metadata, and common Git metadata before network approval. Pre/post destination and credential/config inspections, live symbolic remote `HEAD`, protection authority, GitHub branch metadata, and the final lease-protected delete all use that descriptor-backed context; replacement repository path swaps cannot redirect the operation. GitHub branch metadata is used for GitHub destinations; non-GitHub remotes require the repository-local protection policy.
- Added parser, default/protected/missing/invalid policy, path-swap, and linked-worktree metadata regressions. Gateway tool descriptions and the routed contract snapshot include both delete tools.
