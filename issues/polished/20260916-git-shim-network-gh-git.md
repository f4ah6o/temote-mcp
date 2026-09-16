# Local-agent Git shim slice 4: fetch/pull/push with repo-scoped gh-git identity

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: `20260916-git-shim-worktree.md`
Related: `issues/doing/20260916-repo-scoped-github-account-selection.md`

## Goal

Make ordinary `git fetch`, fast-forward `git pull`, and current-branch `git push` in agent mode use the existing safe host-side contract and repository-scoped `gh-git` credential binding without changing global `gh` account state.

## Scope

- current configured safe remote only;
- fetch --prune, ff-only pull, current branch push / set-upstream equivalent only;
- repo-local approved managed credential mapping only;
- fixed errors for binding missing, credential unavailable, permission denied;
- no raw credential in output/log/argv.

Reject force, arbitrary URL/refspec, config/helper injection, and unknown network Git forms.

## Acceptance

Deterministic tests cover command validation and concurrent repo identity separation. Live GitHub account behavior is recorded in Phase 4; repository-local gates must pass first.
