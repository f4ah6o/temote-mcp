# Linked-worktree broker mutations fail closed in ask/agent because common Git metadata is outside the worktree root

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-temote-managed-worktree-broker.md`
Depends on: `issues/polished/20260917-git-broker-selected-workspace-scope.md`

## Observed on current main + repair round (2026-09-17)

`BrokerScope::for_workspace` **accepts** a linked worktree root: `git_worktree_root(linked) == linked`
because the worktree has a `.git` pointer file, and the pinned identity is
`[<primary>/.git/worktrees/<name>, <primary>/.git]`.

`handle_request` then calls `run_git_command` -> `sandbox::run_git` with writable roots
`[worktree_root]`. `run_git` requires every `git_metadata_roots(cwd)` entry to be contained by
`[cwd] + writable_roots`; the primary repository's `.git` is not, so a normal (ask/agent) session
is rejected before any Git process starts:

```text
Git metadata root is outside the permitted session roots: /tmp/.tmp5OYZz8/selected/.git
```

Reproduced by setting the broker test session to `config::PermissionMode::Agent`; the yolo path
(`sandbox::run_unrestricted`) succeeds because it skips the containment check.

Positive evidence in this round:

- `agent_git::tests::linked_worktree_root_accepts_scope_and_serves_supported_mutations` PASS —
  `BrokerScope::for_workspace(linked)` succeeds, `switch -c` / `add` / `commit` complete through
  the broker, the primary worktree and branch stay unchanged (yolo fixture session).

## Requested change (one responsibility)

Decide and implement the supported metadata scope for linked-worktree broker mutations without
weakening path containment, for example by validating and adding the Git common directory (and its
`worktrees/<name>` private metadata) to the git command's permitted roots only when the selected
workspace's `git_metadata_roots` already resolve there and the primary metadata root is inside the
session's permitted directories.

The fix must keep:

- `BrokerScope` identity pinning (a request can never move to another repository);
- response authority and queue boundaries;
- `.git` config/hooks/refs protection in `SandboxSpec::git`.

## Not changing in the repair round

The current fail-closed behavior stays: no widening was applied while the contract is undecided.

## Verification

- a non-yolo linked-worktree test that switches/adds/commits inside the worktree while the primary
  working tree and siblings stay unchanged;
- structured `git_switch`/`git_branch_create` behavior on linked worktrees compared for parity;
- `just linux-sandbox-acceptance` and CI host gates.

## Completion condition

Linked-worktree broker mutations succeed in the default `agent` mode with metadata scope limited to
the selected repository's own Git directories, or the product explicitly drops that path with
documented rationale.
