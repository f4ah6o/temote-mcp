# P4–P7: prompt correlation and context coverage

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

P4–P7: prompt correlation and context coverage. Parent: `issues/open/20261001-agent-prompt-observation-ingress.md`.

## 背景

Dependency: P0/P1 durable ingress and verified P2/P3 hooks. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Prompt observations can arrive late or be absent, and context must communicate actual coverage.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Correlate durable prompt events to Task/Execution/conversation only with evidence; project coverage and user-intent refs through context_resolve; apply local worker eligibility/sanitization; run cross-agent continuity gate. Unknown correlation stays explicit.

## 受け入れ条件

- [x] Late events converge without duplicates.
- [x] Unsupported hook stays unavailable/partial.
- [x] Head switch shows provenance and freshness.
- [x] Observer/worker failure does not fail task.
- [x] No hidden prompt is exposed.

## テスト計画

Correlation/replay and resolver fixtures, sanitization tests, cross-agent host gate, Rust/gateway checks.

## リスク

Do not make cloud prompt-body replication a prerequisite.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: late convergence, source provenance/freshness, observer isolation and explicit unsupported hook coverage PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/prompt_ingress.rs; src/observation/resolver.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20261001-agent-prompt-observation-ingress.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.
