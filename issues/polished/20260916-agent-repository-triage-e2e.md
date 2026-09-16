# Agent-mode repository triage E2E

Status: polished
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
