# macOS pinned Git identity inspection

Status: doing

CI run 35709691217, job 106687000070 observed seven failures during
pre-approval `pin_git_repository` identity inspection. macOS canonicalized
`/dev/fd/N` as `/dev/fd/repo` instead of the canonical workspace.

Use the descriptor-derived macOS path only for initial identity inspection,
with descriptor and candidate device/inode validation before and after
inspection.

Do not weaken descriptor-backed post-approval Git execution or its safety
checks. Linux and macOS CI are required.

Post-approval runtime behavior is NOT VERIFIED until CI passes.

## Follow-up verification — 2026-09-22

At `be2efb6`, CI run `35712942259` passed the complete Linux job (`106697619409`) and the gateway job (`106697619315`). The macOS job (`106697619369`) passed build checks and clippy but failed seven binary tests: 1010 passed, 7 failed, 2 ignored.

The failure has progressed beyond the initial `/dev/fd/repo` identity inspection. Git itself now rejects the descriptor-backed metadata path with `fatal: not a git repository: '/dev/fd/18'` (descriptor number varies). Dependent remote inspection also fails, including the PR handler integration test with `configured Git remote is unavailable`.

Required remaining gates are the six failing cleanup/branch-delete tests and `github_pr_tools_stay_on_the_configured_repository_and_never_echo_credentials`. This is not solved by weakening tests, dropping macOS coverage, or replacing pinned post-approval authority with a mutable pathname. Keep this issue in `doing` until a safe platform-specific implementation passes those gates.

## Descriptor-proven path projection — 2026-09-22 (Devin session)

`run_pinned_git_command` no longer passes `/dev/fd/N` to Git on macOS:

- `GIT_DIR` / `GIT_COMMON_DIR` are set to each pinned metadata descriptor's `F_GETPATH`-derived real path, proven identical to the descriptor by device/inode immediately before spawn (`validated_fd_path`).
- `GIT_WORK_TREE` is `"."`, bound to the `fchdir(worktree_fd)` child cwd, so the worktree needs no pathname at all.
- After the command exits, both metadata paths are re-proven against their descriptors (`revalidate_fd_path`); a path rebound around the run fails closed instead of silently redirecting the completed operation.
- Linux `/proc/self/fd/N` handling, the pre-approval identity inspection, approval-time pinning, and all `git_push`/lease semantics are unchanged. No test was weakened and no mutable pathname is adopted as authority: the descriptor remains the authority, the path only its re-verified projection.

Verification on a Linux development VM (`38dc76e` + this change): `cargo check --all-targets --all-features` PASS; `cargo check --target aarch64-apple-darwin --no-default-features` and `cargo clippy --target aarch64-apple-darwin --no-default-features` PASS with no new warnings (full-features macOS check cannot run here: `aws-lc-sys` needs an Apple toolchain); focused Linux tests `branch_delete` 5/6 and `agent_git` 42/42 PASS — the one Linux failure and `github_pr_tools_*` failure are the preexisting Devin-VM github.com proxy-rewrite limitation, identical on clean `main`.

Remaining gate: macOS CI on the implementing PR must pass the six cleanup/branch-delete tests and `github_pr_tools_stay_on_the_configured_repository_and_never_echo_credentials`.

Exact final CI evidence: `docs/evaluations/20260922-interrupted-opencode-recovery-review.md`.

No deployment.
