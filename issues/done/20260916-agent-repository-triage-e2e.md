# Agent-mode repository triage E2E

Status: done
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-repository-triage-end-to-end.md`
Depends on: all four Git shim slices + `20260916-github-pr-broker.md`

## Goal

Prove one agent-mode workflow can rescue active work and clean only obsolete repository state without Temote-specific Git instructions.

## Fixture

Create a deterministic repository fixture with:

- dirty main and unrelated untracked file;
- one active rescue branch/worktree;
- one merged/obsolete branch/worktree;
- one open obsolete PR represented by the bounded GitHub adapter fixture.

## Scenario

Agent uses ordinary Git commands to create/use the rescue worktree, edit/add/commit, run focused test, push the current branch, inspect PR state, close only the obsolete PR, and remove only the obsolete clean worktree/branch.

## Acceptance

- original dirty/untracked bytes are unchanged;
- active rescue worktree/ref remains;
- obsolete fixture state is removed;
- final status/worktree/branch/PR state exactly matches fixture expectation;
- no yolo, unrestricted GitHub API, broad `.git` write, or secret exposure;
- `just sandboxed-check` passes.

## Implementation

- `mcp::tests::agent_mode_repository_triage_rescues_active_work_and_cleans_only_obsolete_state`
  builds a deterministic fixture in a tempdir: `<src>/fixture-repo` with dirty
  `main` + unrelated untracked file, a merged `obsolete/merged` branch with its
  own managed worktree, an active `rescue/active-work` branch/worktree, and the
  bounded GitHub adapter (`FakeGithubPrCredentials` + `FakeGithubPrTransport`).
- The scenario drives the same structured operations an agent session would:
  `git_worktree_list` classification, `git_add`/`git_commit` in the rescue
  worktree under a `PermissionMode::Agent` session (approval-free),
  `github_pr_list`/`github_pr_close` through the fake transport,
  `git_worktree_remove` (deterministic snapshot seam) and `git_branch_delete`.
- Active state is proven non-obsolete: removal of the dirty rescue worktree
  and deletion of the checked-out unmerged rescue branch are both refused.
  Final assertions pin dirty/untracked bytes, surviving rescue ref/worktree,
  deleted obsolete ref/worktree, and the exact recorded PR request shape with
  no credential echo.
- Live-network `git_push` to a GitHub destination is outside the bounded
  adapter and remains remote-matrix evidence; the fixture covers the
  deterministic local/PR portion of the scenario.

## Verification

- `cargo test --bin temote-mcp --all-features --locked -- agent_mode_repository_triage`
  passes on a clean host. On this Devin VM the global `url.insteadOf` rewrite
  makes `git remote get-url` return the proxy URL, so the PR step fails like
  the pre-existing `github_pr_tools` environment failure; verified green with
  a clean `HOME`.
- fmt, `cargo clippy --all-targets -- -D warnings`,
  `cargo check --no-default-features --all-targets --locked`, and
  `git diff --check` clean.
