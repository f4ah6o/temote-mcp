# Decide the supported app-server incompatible-protocol contract (no version allowlist)

Status: done — option A verified at `a3591d7`.
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/polished/20260917-appserver-current-main-failure-audit.md`
Depends on: parent audit

## Current code and contract

`validate_initialize_response` (`src/codex_app_server.rs:2017`) checks response shape only.
`app_server_version_from_user_agent` (`:2064`) extracts an optional version for diagnostics.
The deliberate contract (`initialize_validation_is_version_agnostic` `:6579`,
`app_server_version_is_best_effort_diagnostic_only` `:6619`) is: no peer-version rejection and no
Codex version allowlist. `issues/doing/20260908-08-codex-delegation-dogfood-and-app-server.md`
records a live `CODEX_APP_SERVER_INCOMPATIBLE` failure from an earlier design.

## Reproduction

`cargo test --bin temote-mcp --locked app_server_rejects_incompatible_and_oversized_protocol`
fails at `:6764` because the fake "incompatible" initialize response is accepted.

## The one responsibility to change

Resolve the test/contract drift with an explicit decision:

- Option A (preferred if the version-agnostic contract stands): rewrite the test into an
  oversized-protocol test plus a version-agnostic acceptance assertion, and document that a peer
  version is never rejected. No new rejection path.
- Option B (only if rejection is a supported product behavior): define a version-agnostic rejection
  signal (for example a malformed/unsupported initialize shape), not a version allowlist; add one
  focused test for that shape and keep the version-agnostic positive test.

The oversized half of the existing test (`assert!(error.to_string().contains("message exceeds"))`)
must keep passing either way.

## Not changing

- No Codex version allowlist, no assertion deletion, no `#[ignore]`, no retry.
- App-server RPC framing and initialize response shape validation stay.

## Focused tests

- oversized line rejection (existing assertion) passes;
- version-agnostic acceptance for arbitrary peer versions stays asserted;
- if Option B, the chosen malformed/unsupported shape is rejected with a stable classification.

## Completion condition

One option is implemented, the decision is recorded in this issue, and the focused tests pass.

## Decision and verification — 2026-09-22

Option A is implemented: arbitrary peer versions remain diagnostic only, while oversized protocol frames are rejected. The current test is `app_server_accepts_arbitrary_peer_version_and_rejects_oversized_protocol`. It and the version-agnostic/best-effort diagnostic tests pass in Linux and macOS CI run `35709691217`. No version allowlist, ignored assertion, or production rejection policy was introduced. Source and CI review: `docs/evaluations/20260922-interrupted-opencode-recovery-review.md`.
