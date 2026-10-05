# Dogfood: stale metadata and Fabric Link Codex PATH failures

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Dogfood: stale metadata and Fabric Link Codex PATH failures. Parent: `issues/open/20260908-live-acceptance-matrix.md`.

## 背景

Dependency: None; two independently reproduced symptoms require separate diagnoses. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

Observed dogfood found (1) stale supervisor-owned metadata where missing `mbt` caused `session_list` to fail globally and (2) Codex unavailable from Fabric Link process PATH. They are separate defects.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

For (1), isolate per-entry metadata/read errors so healthy sessions remain discoverable with a bounded degraded entry and no implicit deletion. For (2), inspect Fabric Link launch environment and resolve Codex through a documented configured executable or validated PATH; report capability unavailable with actionable diagnostics. Preserve current child approval and owner scope.

## 受け入れ条件

- [ ] A stale `mbt` record does not abort healthy `session_list`.
- [ ] The stale metadata entry is reported as degraded without implicit deletion.
- [ ] When Codex is installed but absent from the Fabric Link process PATH, status explains the resolution failure.
- [ ] A configured, validated Codex executable works without broadening PATH or leaking paths or secrets.

## テスト計画

Separate regression fixtures for stale metadata and Link PATH; real Link canary when available; Rust/gateway gates.

## リスク

The first symptom resembles but is not proven identical to the old missing-cwd bug; do not reuse its completion claim.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260908-live-acceptance-matrix.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
