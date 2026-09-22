# Add bounded host-side GitHub PR list/get/close operations

Status: doing — implementation recovered; full Rust CI verification pending
Implementation: interrupted OpenCode work, completed by delegated Codex implementation workers under coordinator review
Parent: `issues/open/20260916-agent-mode-repository-triage-end-to-end.md`
Depends on: `20260916-git-shim-network-gh-git.md`

## Goal

Provide the minimum GitHub PR capability needed for repository triage without giving the local agent unrestricted GitHub network/API access.

## Scope

- configured current repository only;
- list open PR summaries with bounded fields;
- get one exact PR number;
- close one exact PR number;
- use the same repo-scoped managed credential selection contract as structured GitHub workflow operations;
- return fixed bounded errors and no raw token/header.

Do not implement arbitrary REST endpoints, merge, review, comment, release, issue mutation, or cross-repository access in this packet.

## Acceptance

Pure/PBT tests prove repository/PR-number containment, response bounding, secret non-echo, and no global `gh auth` mutation. Live close is Phase 4/triage fixture acceptance.

## Recovery review — 2026-09-22

- Preserved and completed the interrupted list/get/close implementation and activity events.
- Added strict handler argument checks, canonical PR numbers, required boolean draft state, open-only list validation, configured-repository response identity checks, fixed secret-free error chains, and streaming response bounds.
- Added conservative `limit` / `possibly_truncated` list metadata; this is not a complete-inventory claim.
- Synchronized Rust and gateway tool schemas, snapshots, fingerprints, annotations, and explicit tool-count assertions.
- Added English and Japanese operational references: `docs/github-pr-broker.md` and `docs/ja/github-pr-broker.md`.
- `node gateway/test/github-pr-contract.test.mjs`: PASS, 4 tests.
- `node gateway/test/protocol.test.mjs`: PASS, 64 tests; no tests skipped or removed.
- Formatting and whitespace checks are required before committing this packet.
- Full Rust CI is pending for this packet. The normal local session cannot launch compiler child processes; this restriction is not a compiler result.
- Installed-runtime canary and live fixture PR close: NOT RUN. No real PR, branch, or worktree was deleted.
- Shared macOS descriptor-pinned Git identity failures are tracked separately and must not be hidden by weakening approval or repository identity checks.

## CI review and repair

CI run `35711560164` at `23bcd28` passed formatting, all-target and no-default builds, clippy, the gateway job, and real Linux sandbox acceptance. The Linux binary suite reported 1026 passed, 2 failed, and 1 ignored.

The two failures were reviewed against implementation semantics rather than changing assertions to match the implementation:

- `github_pr_close` waits for a validated closed response, so its activity result must be `Completed`, not `Accepted`. The existing accepted-operation expectation is retained.
- The pure path validator must itself reject relative traversal and absolute paths. Transport-level guards already rejected literal `..`; the helper is strengthened to cover bounded segments and encoded dot/slash/control forms while retaining valid workflow and encoded branch names.

Delegated follow-up implementation adds regression tests for both repairs. The next commit's CI is required; earlier results do not certify that commit. No live PR mutation was used for validation.
