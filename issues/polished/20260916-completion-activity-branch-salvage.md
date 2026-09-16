# Salvage only missing activity work from 2026-09-15 completion branches

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/doing/20260914-local-activity-viewer.md`
Depends on: Phase 1 repository-local friction packets
Source branch: `codex/20260915-completion-activity`

## Goal

Compare current `main` semantically with the old completion-activity branch and port only still-missing S05-S16 behavior. Do not merge the branch.

## Work packet

1. Build a checklist mapping S05-S16 requirements to current-main symbols/tests/docs.
2. Mark each requirement `already-covered` or `missing` with file/test evidence.
3. For the smallest coherent missing behavior, port it to current main and add focused tests.
4. Repeat as separate commits/iterations; do not import old unrelated issue/doc state.
5. After all missing behavior is covered, run activity-focused tests + gateway if protocol changed + `just sandboxed-check`.

## Acceptance

Parent issue gets a concise current-main completion table. Old branch is then safe for Phase 5 deletion after independent diff review.

## Flash-sized execution rule

This packet is **audit/reconciliation only** for S05-S16. Do not port multiple missing behaviors in this task. Produce a current-main coverage table with exact files/tests/old commits. For every missing coherent behavior, create one new `issues/polished/` child issue with a single change scope and exact focused tests. Parent Temote reviews that issue list before implementation.
