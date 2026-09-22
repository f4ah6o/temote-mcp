# macOS pinned Git identity inspection

Status: done — verified at `9861eea`.

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

Exact final CI evidence: `docs/evaluations/20260922-interrupted-opencode-recovery-review.md`.

## Follow-up implementation — 2026-09-22 (second pass)

macOS has no descriptor-relative filesystem path: per `fd(4)`, `open("/dev/fd/N")` only duplicates the descriptor (like `fcntl(N, F_DUPFD, 0)`), so `/dev/fd/N/HEAD` never resolves and Git's `is_git_directory()` rejects `GIT_DIR=/dev/fd/N`. On macOS the pinned run therefore no longer sets `GIT_DIR`/`GIT_COMMON_DIR`/`GIT_WORK_TREE`; the pre-exec `fchdir(worktree_fd)` remains the sole descriptor pin and Git discovers the repository from the anchored cwd. Before spawn, `ensure_pinned_git_discovery` re-proves that `.git` inside the pinned worktree still resolves to the pinned metadata descriptors — directory case compares `.git`'s dev/ino against the pinned `git_dir` descriptor via `fstatat(AT_SYMLINK_NOFOLLOW)`; gitfile case re-reads the pointer through `openat(O_NOFOLLOW)` and re-verifies both the resolved gitdir and commondir against the pinned descriptors. A swap fails closed instead of redirecting discovery to another repository; no mutable pathname is handed to Git.

Linux keeps the descriptor-backed env path unchanged.

## Verification — 2026-09-22

The seven gated tests pass on macOS CI. PR #17 run `35750203797`, macOS job `106822051876`: `Test`, `Supervisor upgrade path E2E`, and `CLI session lifecycle E2E` all green at `1e287f1`; the same run's `ubuntu-latest`/`plan` failures were unrelated flakes (an approvals noprop timeout and a cargo-dist installer download returning HTTP 500), cleared on rerun — run `35751523952` at `9861eea` is green across every job.

Two latent test-harness issues surfaced once the suite ran past the first binary and were fixed in `1e287f1` without touching product code:

- Process-boundary e2e spawned the real binary with a raw tempdir as `HOME`/`XDG_STATE_HOME`; on macOS `$TMPDIR` is under `/var/folders` while `/var` symlinks to `/private/var`, so `reservation_directory` correctly rejected the non-canonical state root. `isolate_process` now canonicalizes the state home up front — `supervisor_upgrade_handoff_preserves_active_session_and_pid` passed in job `106822051876`.
- `session_list_remains_bounded_and_deterministic` could race supervisor lifecycle maintenance if the terminal fixture pair was written after the supervisor started; the fixture is now seeded before `spawn_supervisor`, removing the inter-read window. The test passed in the same macOS job.

No deployment.
