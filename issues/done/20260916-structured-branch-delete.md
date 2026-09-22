# Structured Git cleanup slice 2: lease-protected local/remote branch delete

Status: done — independently reviewed on current `main` (2026-09-22)
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

## 2026-09-22 independent review (Devin session, current `main` `38dc76e`)

- Focused tests: `structured_git_branch_delete_is_merged_only_and_worktree_safe`, `structured_git_branch_delete_stays_bound_after_cwd_path_swap`, `structured_git_remote_branch_delete_stays_bound_after_cwd_path_swap`, `branch_delete_authority_parsers_fail_closed_on_ambiguous_state`, `generated_branch_delete_commands_stay_in_heads_namespace` — PASS (5/6).
- `structured_git_remote_branch_delete_requires_exact_tip_and_one_destination` — FAIL on this VM only at the GitHub pushurl case: the environment rewrites `https://github.com/` to the git-manager proxy (`url.insteadOf`), so the expected `GITHUB_CREDENTIAL_MAPPING_ERROR` check is unreachable here. Same documented Devin-VM limitation class as `github_https_*` / `github_pr_tools`; the full Linux CI job `106697619409` (run `35712942259`) covers it green.
- Existing push safety: `git_push_*` focused tests PASS (4/4).
- Gateway parity: both tools present in `gateway/src/protocol.js` PUBLIC_TOOLS.
- `just sandboxed-check` equivalent (fmt-check, sandboxed-test lib/bin filters, clippy, sandboxed-no-default, gateway-sandbox-test, diff-check): PASS. Host/CI-only lines remain NOT RUN as designed.
