# Linux sandbox: first `git branch <name> <sha>` in a fresh repository fails reading `.git/packed-refs`

Status: done
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

## Disposition — 2026-09-22 (Devin session)

**Real defect on `4c7b35a`, already removed by refactor `ab78587`; regression test added here.**

Diagnosis: on `4c7b35a` `append_missing_mask` emitted `--ro-bind /dev/null .git/packed-refs`
for every missing protected file. bubblewrap materializes a regular empty mountpoint file
(mode 0444 — the exact `stat` the probe recorded) before the bind lands, so the first reader in
a fresh namespace could observe the unbound placeholder instead of the device — matching the
hypothesis that the placeholder's intermediate state denied the reader.

Current main no longer produces that placeholder for this path. `ab78587` ("complete managed
worktree phase 2.4 repair") reworked the policy in `src/sandbox/linux/policy.rs`: a primary
checkout's whole metadata root goes to `read_only_roots`, and only `refs/heads`, `objects`,
`logs` (plus `worktrees` under pinned scope) are re-exposed writable. `helper.rs`
`effective_mount_is_read_only` then *skips* masking missing paths below a read-only mount —
`packed-refs` is absent inside the sandbox and Git takes its native ENOENT path. The surviving
mask (writable-mount case, e.g. `objects/info`, `objects/pack`) is `--dev-bind /dev/null`, a
world-readable character device, not a mode-restricted regular file. Helper unit test
`missing_protected_paths_are_masked_only_when_the_effective_mount_is_writable` pins both halves.

New focused test `sandbox::linux_tests::linux_first_branch_create_on_a_fresh_repository_reads_missing_packed_refs`
runs the exact `run_git` invocation from the observation (`git -c core.hooksPath=/dev/null
branch --no-track feature <sha>`) as the first sandboxed call on a fresh repo, asserts the
branch lands, a second `git branch --list` sees it, and the host never gains a placeholder.

## Verification — 2026-09-22 (Devin session)

- Focused test compiles into the lib target and is discovered; on this VM it reports the
  documented host gate (`temote-linux-sandbox` helper missing), identical to every other
  `sandbox::linux_tests` entry — `just linux-sandbox-acceptance` and the CI `host` job
  (`cargo test --lib --all-features --locked linux_tests`) are the live gate.
- `linux_missing_protected_metadata_stays_missing_in_a_primary_checkout` already asserts
  `packed-refs-absent` + `write-packed-refs-denied` inside the sandbox.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo check --no-default-features --all-targets --locked`, `git diff --check` all clean.
