# macOS canonical test root

Status: done — verified at `a3591d7`.

macOS CI run [35707772355](https://github.com/f4ah6o/temote-mcp/actions/runs/35707772355),
job [106680747242](https://github.com/f4ah6o/temote-mcp/actions/runs/35707772355/job/106680747242),
reported 36 test failures after build and clippy passed. Most failures reported:
`Temote state directory must be canonical and not a swapped path: /tmp/tm22ec-f5da7e/state/temote-mcp`.

macOS canonicalizes `/tmp` to `/private/tmp`. The test-only private process
root now canonicalizes its created path before returning it, preserving the
production managed-worktree guard and its authority.

Linux baseline whole CI: PASS.
New macOS fixture gates: PASS in CI run `35709691217`, job `106687000070`.
Both canonical-root tests pass. Linux CI job `106687000096` passes in full.
See `docs/evaluations/20260922-interrupted-opencode-recovery-review.md`.

This fix does not claim to resolve the full 36-failure cohort. Other unfixed
failures include fd-relative Git paths and the non-UTF-8 filename test.
