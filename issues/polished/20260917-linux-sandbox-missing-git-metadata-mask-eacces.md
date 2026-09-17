# Linux sandbox: first `git branch <name> <sha>` in a fresh repository fails reading `.git/packed-refs`

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/polished/20260916-structured-git-branch-worktree-operations.md` (regression evidence)
Depends on: host Linux sandbox acceptance

## Observed (current main `4c7b35a`, 2026-09-17)

While adding a wiring test, the unchanged structured path
`sandbox::run_git(&mcp::build_git_branch_create_command(branch, base), repo, [repo], &git_metadata_roots, None)`
failed deterministically on this host:

```text
fatal: couldn't read .git/packed-refs: Permission denied
```

Probe facts:

- fresh `git init` + one commit via host git (no `.git/packed-refs` on the host);
- `sandbox::run_git` with `/usr/bin/git -c core.hooksPath=/dev/null branch --no-track <name> <sha>` fails
  (status 128, the message above) on the first invocation;
- the same sandbox run of `git branch --list` succeeds;
- `sandbox::run_git` with `stat -c "%F %a %s" .git/packed-refs` reports `regular empty file 444 0`
  (the missing-metadata mask exists inside the sandbox);
- after that first sandbox run, the second `git branch <name> <sha>` invocation succeeds;
- `sandbox::linux_tests::linux_normal_git_metadata_is_read_only_but_run_git_can_commit` passes
  because `add`/`commit` do not read `packed-refs`.

Hypothesis to confirm, not yet proven: the helper's missing-path mask
(`append_missing_mask`, `src/sandbox/linux/helper.rs:322`) creates the placeholder mountpoint with
an intermediate mode that denies the reader until a later bind replaces it, so the first reader in
a fresh namespace can lose a race with helpers that do not read that path.

## Reproduction

A temporary probe test added during the repair round (then removed) ran
`mcp::build_git_base_commit` + `mcp::build_git_branch_create_command` through `sandbox::run_git` on a
fresh repository and reproduced the failure deterministically; the same command shape is what the
structured `git_branch_create` tool uses.

## Requested change (one responsibility)

Determine whether the mask ordering/mode is a real helper defect and fix it so the first sandboxed
`git branch` create reads `packed-refs` as a read-only empty file, without widening Git metadata
write access. If it is host-only (for example an AppArmor/bwrap interaction), record the supported
environment and the CI/host gate that proves it.

## Not changing

- Structured Git tools, broker scope, or sandbox writable roots.
- Existing metadata read-only invariants (`config`, `hooks`, `packed-refs` stay non-writable).

## Verification

- focused sandbox test that creates a branch through `run_git` on a fresh repository, first
  invocation included;
- `just linux-sandbox-acceptance` on a suitable host;
- CI Linux live sandbox acceptance.

## Completion condition

First-invocation branch creation succeeds on the supported Linux sandbox, or the behavior is
explicitly documented as host-environment-limited with evidence.
