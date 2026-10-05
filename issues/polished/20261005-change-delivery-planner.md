# D3: deterministic Change delivery plan

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

D3: deterministic Change delivery plan. Parent: `issues/open/20260926-task-change-orchestration-stacked-pr.md`.

## 背景

Dependency: D1/D2 and revision-bound verification. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

Delivery topology cannot be inferred from task hierarchy or stale working branches.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Plan single, sibling or stacked delivery solely from explicit Change dependency edges; bind verified revision, materialized ref and base revision; persist deterministic plan fingerprint and flag parent merge/rebase staleness before delivery.

## 受け入れ条件

- [ ] Equivalent graph yields one plan/fingerprint.
- [ ] Dependency cycle or missing verified revision fails.
- [ ] Changed parent revision makes plan stale.
- [ ] Executor tree variations leave delivery topology unchanged.

## テスト計画

Graph model/property fixtures, stale-base and revision-binding tests; Rust gates.

## リスク

Do not mark execution completion as verification PASS or delivery done.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260926-task-change-orchestration-stacked-pr.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
