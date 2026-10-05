# F1: scoped friction candidate consumer

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

F1: scoped friction candidate consumer. Parent: `issues/open/20260926-observation-friction-worker.md`.

## 背景

Dependency: Existing observation journal and cloud memory baseline. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

Recurring Temote friction is observable but not classified into supported candidates.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Use independent checkpoint/fencing per full session instance or Fabric owner/repository sequence; scan bounded episodes and classify temote_friction, target_repository_bug, upstream_transient, insufficient_evidence or known_existing_issue. Retain support refs, facts versus hypotheses, and recurrence.

## 受け入れ条件

- [ ] Replay/restart produces one candidate.
- [ ] Same-name replacement cannot inherit cursor.
- [ ] Unsupported or expired evidence is not publishable.
- [ ] One degraded source does not stop others.
- [ ] Memory checkpoint and coding-task state stay independent.

## テスト計画

Deterministic episode/classification, replay, corrupt-source and generation-fence tests; Rust/gateway gates.

## リスク

Never publish private transcripts from a candidate.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260926-observation-friction-worker.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
