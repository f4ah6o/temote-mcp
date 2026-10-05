# D2: Change allocation and isolated writer handoff

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

D2: Change allocation and isolated writer handoff. Parent: `issues/open/20260926-task-change-orchestration-stacked-pr.md`.

## 背景

Dependency: D1 Change record and S2 workspace allocator. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

Mutating delegates lack an explicit Change allocation and can confuse agent hierarchy with writable change identity.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Allocate or reuse a durable Change before mutating execution; accept explicit base_change; ensure isolated workspace; permit same-Change executor replacement through fenced writer handoff. Task/Execution/Change/Workspace identities remain distinct.

## 受け入れ条件

- [ ] Two sibling changes never share one mutable workspace.
- [ ] Replacement executor keeps ChangeId and does not duplicate allocation.
- [ ] Base_change conflicts fail.
- [ ] Agent/subagent creation alone makes no Change or PR.

## テスト計画

Concurrent allocation, same-key retry, generation handoff and lost-response tests; Rust gates.

## リスク

Preserve the single-writer reservation and no-local-main contract.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260926-task-change-orchestration-stacked-pr.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
