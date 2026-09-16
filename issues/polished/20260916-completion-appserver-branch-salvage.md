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
