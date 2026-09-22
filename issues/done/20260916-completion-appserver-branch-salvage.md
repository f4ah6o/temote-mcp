# Salvage only missing Codex app-server runtime fixes from completion branches

Status: done
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


## Implementation

2026-09-22 audit, no code ported. Sources `codex/20260915-completion-appserver` and `codex/20260915-test-runtime-isolation` no longer exist as refs; their history was audited via `codex/20260915-complete-open-work` (which carries their merges) plus `codex/eval-t05-c-r2-incomplete`. Every repository-local runtime ownership/fencing/cleanup, protocol/user-agent, and crash/reconcile/retention behavior is already covered on `main` — current `main` is a strict superset of the old branches' app-server surface. The parent now carries the per-item evidence table (`## 2026-09-22 current-main coverage (salvage audit)`).

The drift recorded in this packet's observation section is resolved: `app_server_rejects_incompatible_and_oversized_protocol` was deliberately superseded by `app_server_accepts_arbitrary_peer_version_and_rejects_oversized_protocol` under the version-agnostic contract (`app_server_version_is_best_effort_diagnostic_only`; `CODEX_APP_SERVER_INCOMPATIBLE` removed by design), and `cross_process_store_and_runtime_ownership_are_fenced` passes on current `main`. The only branch-only functional delta found, a `#[cfg(test)]` override of `resolve_codex_home` pointing tests at a process-private Codex home, is dead isolation on `main` — tests never invoke the `*_current` wrappers that call it; all tests pass explicit tempdirs.

Missing coherent behaviors: none. No child issues were created. The old app-server/runtime-isolation branch history is safe for Phase 5 deletion after independent diff review; Phase 4 live dogfood/comparison/adoption stays open on the parent.

## Verification

- `cargo test --bin temote-mcp --all-features --locked -- cross_process_store_and_runtime_ownership_are_fenced` → 1 passed (repro of the recorded drift).
- `git diff main origin/codex/20260915-complete-open-work -- src/codex_app_server.rs src/codex.rs src/local_agent.rs src/dev_tool.rs src/http.rs` — all branch-only deltas are stale supersets of what `main` now covers or evidence/test scaffolding; no portable missing behavior.
- `git diff --check` clean; issue-only change, no Rust/gateway gates apply.
