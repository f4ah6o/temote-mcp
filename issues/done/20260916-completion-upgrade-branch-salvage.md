# Salvage only missing durable-upgrade work from 2026-09-15 branch

Status: done
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/doing/20260908-07-client-safe-upgrade-reconnect.md`
Depends on: activity salvage packet if shared activity contracts are touched
Source branch: `codex/20260915-completion-upgrade`

## Goal

Port only upgrade behavior still absent from current `main`; never merge the old branch tree.

## Work packet

Map current main against the parent's remaining items: direct HTTP coordinator, response-delivery commit barrier, remote preflight/apply/status, idempotency/stale transaction behavior, process-boundary reconnect E2E, and docs. For each item, prove current-main coverage or port one bounded missing slice with focused tests.

Linux/macOS live reconnect acceptance remains Phase 4 and must not be marked PASS from deterministic fixtures.

## Acceptance

All repository-local remaining items are covered on current main, strict upgrade tests and `just sandboxed-check` pass, and the parent records exact live-only residuals.

## Flash-sized execution rule

This packet is **audit/reconciliation only** for upgrade residuals. Do not port multiple missing behaviors in this task. Produce a current-main coverage table with exact files/tests/old commits. For every missing coherent behavior, create one new `issues/polished/` child issue with a single change scope and exact focused tests. Parent Temote reviews that issue list before implementation.


## Implementation

2026-09-22 audit, no code ported. `codex/20260915-completion-upgrade` is deleted as a ref; its history was audited via `codex/20260915-complete-open-work`. Every repository-local remaining item from the parent's acceptance list — durable transaction schema, boot identity, coordinator primitives, detached coordinator ownership, response-flush/commit barrier, remote preflight/apply/status tools, idempotency and stale-transaction handling, credential-free persistence, and EN/JA + Agent Skill docs — is already covered on `main`, a strict superset of the old branch.

The only residual is the macOS process-boundary reconnect E2E: `tests/upgrade_reconnect_e2e.rs` covers Linux behind `#[ignore]` as an explicit host gate, and per this packet's rule the macOS leg remains Phase 4 live acceptance rather than deterministic fixture work. It is recorded on the parent as the exact live-only residual.

Branch-only deltas are stale supersets or test scaffolding (`#[cfg(test)]` overrides of `default_runtime_directory`/`resolve_codex_home` — dead isolation on `main` because the only implicit callers are production paths and `config::state_dir` is already test-isolated, plus an older 8 MiB `MAX_MCP_RESPONSE_BYTES` now 52 MiB) and docs owned by the evidence packet. Missing coherent behaviors: none. No child issues were created. The old upgrade branch history is safe for Phase 5 deletion after independent diff review.

## Verification

- `git diff main origin/codex/20260915-complete-open-work -- src/upgrade_transaction.rs src/upgrade_coordinator.rs src/lifecycle.rs src/http.rs src/cli.rs tests/` — no portable missing behavior.
- Parent acceptance items cross-checked against current-main tools, tests, and docs; per-item evidence table appended to the parent as `## 2026-09-22 current-main coverage (salvage audit)`.
- `git diff --check` clean; issue-only change, no Rust/gateway gates apply.
