# V3: reconcile jj receipts and bind snapshots to task boundaries

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

V3: reconcile jj receipts and bind snapshots to task boundaries. Parent: `issues/open/20260925-vcs-transaction-jj-first.md`.

## 背景

Dependency: V3 first slice merged in PR #59. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Accepted snapshots can remain uncertain, and task edits are not yet automatically correlated with durable VCS observations.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Implement vcs_reconcile for accepted receipts; persist Task/Execution/VCS correlation and observation emission; snapshot at defined task/verification boundaries; provide controlled workspace release only after retained work is safe. `auto` jj unsupported reports capability error and never silently selects Git.

## 受け入れ条件

- [x] Lost response replays one snapshot.
- [x] Crash recovery returns the existing result or reconciliation_required.
- [x] Observation references the same task, execution and before/after revision.
- [ ] Task-boundary edits are recoverable without agent commits.
- [x] Release refuses unreconciled work.
- [x] Unsupported jj auto never falls back.

## テスト計画

Focused receipt/crash tests, two-workspace isolation, observation round-trip and explicit Git compatibility fixture; Rust gates and host jj acceptance.

## リスク

Protect dirty or ahead existing checkouts and all operation receipts.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Delegated before/after snapshot receipt, lost-response, crash, owner/CAS and bound-admission fixtures passed. Actual jj boundary edit recovery remains NOT RUN.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260925-vcs-transaction-jj-first.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Delegated before/after snapshot receipt, lost-response, crash, owner/CAS and bound-admission fixtures passed. Actual jj boundary edit recovery remains NOT RUN.
