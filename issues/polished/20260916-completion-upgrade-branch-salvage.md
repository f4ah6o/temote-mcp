# Salvage only missing durable-upgrade work from 2026-09-15 branch

Status: polished
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
