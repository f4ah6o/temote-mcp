# F2: authorized friction publication and reconciliation

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

F2: authorized friction publication and reconciliation. Parent: `issues/open/20260926-observation-friction-worker.md`.

## 背景

Dependency: F1 candidates and explicit repository publication authority. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

A supported candidate could be lost after a failed publish or create duplicate PRs after a lost response.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Separate publisher with Temote-repo Git/GitHub write authorization and export/redaction policy; search scoped fingerprint and existing receipts; create or update local issue in a dedicated workspace and focused review PR. Persist outbox/receipts before external effects and reconcile uncertain results.

## 受け入れ条件

- [x] Missing authority or stale support retains candidate.
- [x] Duplicate request creates at most one logical issue/PR.
- [ ] Crash after branch/push/PR reconciles.
- [x] Generated PR remains unmerged pending review.

## テスト計画

Publisher fault-injection, redaction, duplicate and GitHub mock tests; live publishing gate separately recorded.

## リスク

No cross-repository private content or automatic merge.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Authority/full-instance/replay/read-only observation fixtures passed; live branch/push/PR crash reconciliation remains NOT RUN.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260926-observation-friction-worker.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Authority/full-instance/replay/read-only observation fixtures passed; live branch/push/PR crash reconciliation remains NOT RUN.
