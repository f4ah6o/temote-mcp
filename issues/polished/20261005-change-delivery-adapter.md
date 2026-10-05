# D4/D5: reconciled GitHub and gh-stack delivery

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

D4/D5: reconciled GitHub and gh-stack delivery. Parent: `issues/open/20260926-task-change-orchestration-stacked-pr.md`.

## 背景

Dependency: D3 deterministic plan, V3 snapshot wiring. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

A remote response loss can duplicate PRs, and delivery status may drift from the verified Change revision.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Use bounded GitHub/gh-stack adapters for single and stacked PRs; persist Accepted receipts before remote effects; reconcile expected and actual PR/ref graph after uncertainty; bind final verification to delivered revision; block workspace release while delivery is unreconciled.

## 受け入れ条件

- [ ] Single and stacked plans create intended PR graph once.
- [ ] Repeated key after lost response reconciles existing PR.
- [ ] Stale base requires replan/reverification.
- [ ] Delivery remains not_started until a delivery operation records it.

## テスト計画

Disposable GitHub fixture/mock reconciliation tests, gh-stack versioned CLI contract, Rust/gateway gates, live gate NOT RUN until authorized environment.

## リスク

No blind remote retries, automatic merge, or mutation of sibling worktrees.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260926-task-change-orchestration-stacked-pr.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
