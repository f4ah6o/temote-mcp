# D2: Change allocation and isolated writer handoff

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

D2: Change allocation and isolated writer handoff. Parent: `issues/open/20260926-task-change-orchestration-stacked-pr.md`.

## 背景

Dependency: D1 Change record and S2 workspace allocator. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

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
- [x] Replacement executor keeps ChangeId and does not duplicate allocation.
- [x] Base_change conflicts fail.
- [x] Agent/subagent creation alone makes no Change or PR.

## テスト計画

Concurrent allocation, same-key retry, generation handoff and lost-response tests; Rust gates.

## リスク

Preserve the single-writer reservation and no-local-main contract.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Fresh post-repair canary selected visible default `gpt-6.1-sol` / `low`. A valid native failed report retained an initialized isolated bare store and failed marker after DNS/network approval failure. No workspace/ready marker or active generated session resulted. Model selection passed; source provisioning remains BLOCKED, and physical allocation/environment acceptance remains NOT RUN.

- 2026-10-06: Actual isolated managed-source canary reached a valid native BLOCKED report because child network approval for HTTPS fetch was rejected. No repository, workspace or readiness marker was created; provisioning did not activate. Fetch retry, physical allocation and downstream environment acceptance remain NOT RUN. Automatic model selection was corrected to preserve visible catalog default metadata; no approval boundary was widened.

- 2026-10-06: Allocation/CAS/writer fixtures passed. Physical sibling jj workspace handoff is NOT RUN pending managed-host acceptance.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260926-task-change-orchestration-stacked-pr.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Allocation/CAS/writer fixtures passed. Physical sibling jj workspace handoff is NOT RUN pending managed-host acceptance.
