# S2: named-root workspace allocation and jj binding

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

S2: named-root workspace allocation and jj binding. Parent: `issues/open/20260929-session-first-managed-provisioning.md`.

## 背景

Dependency: RepositoryStore ensure and named-root admission. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

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
- [ ] Same accepted retry resolves the same allocation.
- [ ] Writer handoff is fenced by full instance/generation.
- [ ] Workspace paths cannot escape the named root.
- [ ] Git compatibility requires explicit selection.

## テスト計画

Allocator race, symlink, replay, restart and stale-owner tests; jj host fixture and Rust gates.

## リスク

Never delete or reset old managed/unmanaged worktrees to satisfy allocation.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260929-session-first-managed-provisioning.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
