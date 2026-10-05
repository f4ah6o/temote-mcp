# D3: deterministic Change delivery plan

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

D3: deterministic Change delivery plan. Parent: `issues/open/20260926-task-change-orchestration-stacked-pr.md`.

## 背景

Dependency: D1/D2 and revision-bound verification. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Delivery topology cannot be inferred from task hierarchy or stale working branches.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Plan single, sibling or stacked delivery solely from explicit Change dependency edges; bind verified revision, materialized ref and base revision; persist deterministic plan fingerprint and flag parent merge/rebase staleness before delivery.

## 受け入れ条件

- [x] Equivalent graph yields one plan/fingerprint.
- [x] Dependency cycle or missing verified revision fails.
- [x] Changed parent revision makes plan stale.
- [x] Executor tree variations leave delivery topology unchanged.

## テスト計画

Graph model/property fixtures, stale-base and revision-binding tests; Rust gates.

## リスク

Do not mark execution completion as verification PASS or delivery done.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: reference-model graph fingerprint, cycles/missing verification, parent staleness and execution-independent topology PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/delivery.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260926-task-change-orchestration-stacked-pr.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.
