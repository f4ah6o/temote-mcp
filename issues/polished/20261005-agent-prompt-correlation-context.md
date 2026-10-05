# P4–P7: prompt correlation and context coverage

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

P4–P7: prompt correlation and context coverage. Parent: `issues/open/20261001-agent-prompt-observation-ingress.md`.

## 背景

Dependency: P0/P1 durable ingress and verified P2/P3 hooks. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

Prompt observations can arrive late or be absent, and context must communicate actual coverage.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Correlate durable prompt events to Task/Execution/conversation only with evidence; project coverage and user-intent refs through context_resolve; apply local worker eligibility/sanitization; run cross-agent continuity gate. Unknown correlation stays explicit.

## 受け入れ条件

- [ ] Late events converge without duplicates.
- [ ] Unsupported hook stays unavailable/partial.
- [ ] Head switch shows provenance and freshness.
- [ ] Observer/worker failure does not fail task.
- [ ] No hidden prompt is exposed.

## テスト計画

Correlation/replay and resolver fixtures, sanitization tests, cross-agent host gate, Rust/gateway checks.

## リスク

Do not make cloud prompt-body replication a prerequisite.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20261001-agent-prompt-observation-ingress.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
