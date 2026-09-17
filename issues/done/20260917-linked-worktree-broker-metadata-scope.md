# Linked-worktree broker mutations fail closed in ask/agent because common Git metadata is outside the worktree root

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-temote-managed-worktree-broker.md`
Depends on: `issues/done/20260917-git-broker-selected-workspace-scope.md`

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

## Implementation notes (2026-09-17)

`src/agent_git.rs`:

- `RepositoryIdentity` now also pins `git_primary_checkout` and a `linked_worktree` flag
  (`primary_checkout != worktree_root`); `BrokerScope::for_workspace` fails closed when the
  workspace is not a standard primary or linked worktree root.
- `run_git_command` routes a linked worktree through the new
  `sandbox::run_git_with_pinned_worktree_metadata` entry point with the pinned
  `git_metadata_roots`. A primary checkout keeps the contained `sandbox::run_git` path.
- `resolve_cwd` still re-derives `git_worktree_root`, `git_metadata_roots` and now
  `git_primary_checkout` for every request, so a swapped or symlinked `.git` pointer fails
  closed before any Git process starts.

`src/sandbox.rs`:

- `run_git` is now a thin wrapper over a scope-checked implementation.
  `GitMetadataScope::Contained` preserves the previous containment rule. The new
  `run_git_with_pinned_worktree_metadata` uses `GitMetadataScope::PinnedWorktree`; it still
  requires `provided_git_metadata_roots == git_metadata_roots(cwd)`, so the only roots that can
  ever leave the writable scope are the exact validated ones derived from the command cwd.
- The `git` metadata policy is unchanged: `config`, `hooks`, `refs/tags`, `refs/remotes`,
  `packed-refs`, `worktrees` and pack/info metadata stay read-only; only `refs/heads`, loose
  objects/logs and the linked private metadata (minus `gitdir`/`commondir`) are writable.

`src/sandbox/linux/helper.rs`:

- Missing protected metadata files were masked with `--ro-bind /dev/null`. Bubblewrap
  read-only binds are `nodev`, so reading the mask failed with `EACCES`; Git treats an
  unreadable optional `packed-refs` (or `shallow`) as fatal. The mask now uses
  `--dev-bind /dev/null`: reads yield empty content, writes are still discarded and no
  writable mountpoint is exposed. This unblocks `git branch`/`git switch` inside the sandbox
  for any repository without `packed-refs`, including ordinary primary checkouts.

Focused tests (repository-local):

- `agent_git::tests::linked_worktree_scope_pins_metadata_below_the_primary_checkout`
- `agent_git::tests::broker_rejects_swapped_and_symlinked_linked_worktree_metadata`
- `sandbox::generic_tests::linked_worktree_metadata_requires_the_pinned_scope`
- existing `agent_git` broker suite, `sandbox` generic tests and `mcp` Git tests

Host acceptance (nested Linux sandbox required):

- `agent_git::tests::agent_mode_linked_worktree_mutations_use_only_the_pinned_metadata`
  (`#[ignore]`, run by `just linux-sandbox-acceptance`): `switch -c`, `add` and `commit`
  through the broker in the default `agent` mode while the primary checkout, its untracked
  sentinel and a sibling worktree stay unchanged.
- `sandbox::linux_tests::linux_missing_protected_metadata_files_stay_readable`
- `sandbox::linux_tests::linux_linked_worktree_pinned_metadata_scope_serves_git_mutations`

Results in this round: `just sandboxed-check` PASS; `just linux-sandbox-acceptance` PASS on a
Linux host with nested bubblewrap; macOS Seatbelt execution NOT RUN.
