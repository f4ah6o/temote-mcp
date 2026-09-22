# Interrupted OpenCode recovery: reviewer evidence

Date: 2026-09-22. Repository: `f4ah6o/temote-mcp`.

## Ownership and preservation

Implementation patches were authored by delegated Codex workers. The coordinator reviewed, rejected incorrect candidates, mechanically integrated accepted patches, formatted, verified, and committed them. The interrupted PR broker changes were preserved. Pre-existing `.tmp/`, `.wt/`, evaluation notes, and unrelated issue files were not discarded.

Repository-local `gh git binding status --json` resolved the repository and credential identity to `f4ah6o`. Pushes used the managed repository binding without changing global `gh` account selection.

## Pushed source checkpoints

- `44fc8c3`: OpenCode standalone CLI compatibility and three exact-argv regression tests.
- `a3591d7`: canonical private test roots on macOS and the portable raw-byte reservation fixture split.
- `23bcd28`: recovered bounded GitHub PR broker, validation, response bounds, activity coverage, gateway contract, and EN/JA references.

## Verified baseline and platform delta

CI run [35707772355](https://github.com/f4ah6o/temote-mcp/actions/runs/35707772355), source `44fc8c3`, passed the Linux and gateway jobs. The macOS build, no-default build, formatting, and clippy passed; its binary tests reported 964 passed, 36 failed, and 2 ignored.

CI run [35709691217](https://github.com/f4ah6o/temote-mcp/actions/runs/35709691217), source `a3591d7`:

- Linux job `106687000096`: SUCCESS, including all-targets/all-features tests, no-default checks, clippy, real Linux sandbox acceptance, supervisor upgrade E2E, CLI lifecycle E2E, dependency boundary, and packaged-crate installation.
- Gateway job `106686999956`: SUCCESS.
- macOS job `106687000070`: format, both build checks, and clippy PASS; library tests 125 passed; binary tests 994 passed, 7 failed, 2 ignored.
- Both canonical-root tests and the cross-Unix raw-byte reservation identity test PASS on macOS. The fixture repairs eliminated 29 failures without changing production path authority.
- The remaining seven failures are descriptor-pinned Git branch-delete/cleanup operations. They are not app-server, worktree remove/prune, or ordinary fetch/pull/push failures. The macOS job as a whole is NOT PASS.

## Issue closure decisions

Newly repaired and verified: `20260922-macos-canonical-test-root.md`, `20260922-macos-non-utf8-reservation-fixture.md`.

Previously implemented, now reviewed against source and passing targeted CI tests: `20260916-structured-worktree-remove.md`, `20260916-managed-worktree-prune.md`, `20260916-git-shim-worktree.md`, `20260916-git-shim-network-gh-git.md`, and `20260917-appserver-compat-contract-decision.md`. These closures do not claim the coordinator newly authored the older implementations.

For these bounded packets, the constituent repository gates were executed in CI: formatting, all-features library and binary tests, no-default build, clippy, and the gateway suite. The literal local `just sandboxed-check` invocation was not successful in this session because compiler child creation was denied; no local success is claimed. Linux CI additionally ran the real sandbox and E2E gates. Live GitHub account-matrix and destructive fixture acceptance remain Phase 4 work and are not inferred from unit tests.

The app-server compatibility decision is option A: peer versions are diagnostic only. `app_server_accepts_arbitrary_peer_version_and_rejects_oversized_protocol`, `initialize_validation_is_version_agnostic`, and `app_server_version_is_best_effort_diagnostic_only` passed. No version allowlist, ignored assertion, or new rejection policy was added.

## PR packet boundary

At `23bcd28`, direct local Node execution passed the new PR contract suite (4 tests) and existing protocol suite (64 tests). Snapshots and fingerprint were regenerated from the gateway definition, without fabricating a digest. Full Rust CI for that packet remains a separate check; earlier commit results are not substituted for it.

CI run `35711560164` subsequently verified the `23bcd28` builds, no-default check, clippy, gateway, and real Linux sandbox acceptance. Its Linux binary tests reported 1026 passed, 2 failed, and 1 ignored. The failures concerned PR close activity classification and the pure path validator's containment contract; they were not waived.

Follow-up source repairs:

- `94621ed`: macOS pre-approval repository identity inspection uses the descriptor-derived path with before/after device/inode checks. Post-approval descriptor-backed Git execution is unchanged. This still requires macOS CI acceptance.
- `5ccc62a`: a confirmed PR close is classified as completed, preserving the original accepted-operation test expectation. The shared pure path validator now rejects empty, absolute, dot-segment, and encoded structural-byte forms while preserving legitimate workflow and encoded branch identifiers. Two regression tests were added by the implementation worker.

These follow-up commits require their own CI results; results from earlier commits are not substituted for them.

## Still not verified

- Rebuilt installed-runtime OpenCode canary and nested agent shell execution.
- Live fixture PR close and full repository triage acceptance.
- The seven macOS descriptor-pinned Git failures and their downstream acceptance.

No production restart, permission bypass, global account switch, real PR close, branch deletion, or unrelated worktree cleanup was performed.
