# S2: named-root workspace allocation and jj binding

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

S2: named-root workspace allocation and jj binding. Parent: `issues/open/20260929-session-first-managed-provisioning.md`.

## 背景

Dependency: RepositoryStore ensure and named-root admission. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

A managed session needs an isolated writable workspace with one active writer and a durable Change binding.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Allocate generated WorkspaceId under a configured named-root-backed pool; reserve owner and scope before mutation; create/reuse jj workspace from the bare store and bind ChangeId. No caller-selected host path and no Git branch/worktree in normal jj session start.

## 受け入れ条件

- [ ] Concurrent independent changes receive isolated workspaces.
- [x] Same accepted retry resolves the same allocation.
- [x] Writer handoff is fenced by full instance/generation.
- [x] Workspace paths cannot escape the named root.
- [x] Git compatibility requires explicit selection.

## テスト計画

Allocator race, symlink, replay, restart and stale-owner tests; jj host fixture and Rust gates.

## リスク

Never delete or reset old managed/unmanaged worktrees to satisfy allocation.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Fresh post-repair canary selected visible default `gpt-6.1-sol` / `low`. A valid native failed report retained an initialized isolated bare store and failed marker after DNS/network approval failure. No workspace/ready marker or active generated session resulted. Model selection passed; source provisioning remains BLOCKED, and physical allocation/environment acceptance remains NOT RUN.

- 2026-10-06: Actual isolated managed-source canary reached a valid native BLOCKED report because child network approval for HTTPS fetch was rejected. No repository, workspace or readiness marker was created; provisioning did not activate. Fetch retry, physical allocation and downstream environment acceptance remain NOT RUN. Automatic model selection was corrected to preserve visible catalog default metadata; no approval boundary was widened.

- 2026-10-06: Allocation/ready marker/race/path/CAS fixtures passed. Live concurrent managed workspaces remain NOT RUN.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260929-session-first-managed-provisioning.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Allocation/ready marker/race/path/CAS fixtures passed. Live concurrent managed workspaces remain NOT RUN.
