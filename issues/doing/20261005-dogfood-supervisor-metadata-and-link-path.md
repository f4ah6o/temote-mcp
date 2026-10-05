# Dogfood: stale metadata and Fabric Link Codex PATH failures

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Dogfood: stale metadata and Fabric Link Codex PATH failures. Parent: `issues/open/20260908-live-acceptance-matrix.md`.

## 背景

Dependency: None; two independently reproduced symptoms require separate diagnoses. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Observed dogfood found (1) stale supervisor-owned metadata where missing `mbt` caused `session_list` to fail globally and (2) Codex unavailable from Fabric Link process PATH. They are separate defects.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

For (1), isolate per-entry metadata/read errors so healthy sessions remain discoverable with a bounded degraded entry and no implicit deletion. For (2), inspect Fabric Link launch environment and resolve Codex through a documented configured executable or validated PATH; report capability unavailable with actionable diagnostics. Preserve current child approval and owner scope.

## 受け入れ条件

- [x] A stale `mbt` record does not abort healthy `session_list`.
- [x] The stale metadata entry is reported as degraded without implicit deletion.
- [x] When Codex is installed but absent from the Fabric Link process PATH, status explains the resolution failure.
- [ ] A configured, validated Codex executable works without broadening PATH or leaking paths or secrets.

## テスト計画

Separate regression fixtures for stale metadata and Link PATH; real Link canary when available; Rust/gateway gates.

## リスク

The first symptom resembles but is not proven identical to the old missing-cwd bug; do not reuse its completion claim.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Degraded metadata fixtures and configured native Codex canary passed. Production Link reload is NOT RUN: authorized 1Password runtime injection unavailable; supervisor handoff blocked by mbt restart context.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260908-live-acceptance-matrix.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Degraded metadata fixtures and configured native Codex canary passed. Production Link reload is NOT RUN: authorized 1Password runtime injection unavailable; supervisor handoff blocked by mbt restart context.
