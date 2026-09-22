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

Exact final CI evidence: `docs/evaluations/20260922-interrupted-opencode-recovery-review.md`.

No deployment.
