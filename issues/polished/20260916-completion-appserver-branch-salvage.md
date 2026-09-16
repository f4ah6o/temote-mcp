# Salvage only missing Codex app-server runtime fixes from completion branches

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/doing/20260908-08-codex-delegation-dogfood-and-app-server.md`
Depends on: Phase 0 rebuilt runtime
Sources: `codex/20260915-completion-appserver`, `codex/20260915-test-runtime-isolation`, `codex/eval-t05-c-r2-incomplete`

## Goal

Review old runtime-isolation/app-server changes against current main and port only missing correctness fixes needed for current supported Codex app-server behavior.

## Scope

- runtime ownership/fencing/cleanup correctness;
- current supported app-server protocol/user-agent compatibility;
- deterministic tests for crash/reconcile/retention boundaries;
- no wholesale branch merge and no resurrection of stale docs/issues.

The incomplete T05-C branch is evidence only unless its code exposes a still-reproducible bug on current main.

## Acceptance

Current-main focused app-server tests pass; parent issue clearly separates repository-local completion from Phase 4 live dogfood/comparison/adoption decision.

## Flash-sized execution rule

This packet is **audit/reconciliation only** for app-server/runtime residuals. Do not port multiple missing behaviors in this task. Produce a current-main coverage table with exact files/tests/old commits. For every missing coherent behavior, create one new `issues/polished/` child issue with a single change scope and exact focused tests. Parent Temote reviews that issue list before implementation.

## Current-main observation (2026-09-16, during Phase 1 session-orphan-gc)

Two `src/codex_app_server.rs` binary tests fail on current `main` (reproduced at `ef9f910` + Phase 1 commits, and also with the session-GC working tree stashed, so they are not caused by that packet):

```text
cargo test --bin temote-mcp --locked codex_app_server::tests::
  app_server_rejects_incompatible_and_oversized_protocol ... FAILED
    panicked at src/codex_app_server.rs:6764: Option::unwrap() on a None value
    (the fake incompatible app-server `spawn_initialized_client_with_binary_mode(...)` returned Ok)
  cross_process_store_and_runtime_ownership_are_fenced ... FAILED
```

Observed facts, not conclusions:

- the test asserts `error.to_string().contains("CODEX_APP_SERVER_INCOMPATIBLE")`, but that string exists nowhere in current `src/` (only in this test);
- current `app_server_version_from_user_agent` treats a user agent carrying `(temote-mcp; <CARGO_PKG_VERSION>)` as compatible and otherwise only degrades the version diagnostic; there is no current rejection path;
- `issues/doing/20260908-08-codex-delegation-dogfood-and-app-server.md` records a live `codex_status` failure with `CODEX_APP_SERVER_INCOMPATIBLE`, so an earlier design produced that classification.

This is repository-local app-server test/protocol drift owned by this salvage packet. Handle it in the audit coverage table; if the supported contract should reject the tested shape, create one bounded child packet with exact focused tests instead of editing the stale assertion in place.
