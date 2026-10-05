# D: scoped environment preparation adapters

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

D: scoped environment preparation adapters. Parent: `issues/open/20260924-temote-development-harness-restructure.md`.

## 背景

Dependency: Managed workspace allocation. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

A workspace can be assigned before its build dependencies and caches are ready.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add workspace ready-state and bounded adapters for installed vp/pnpm/Cargo/sccache behavior; pin tool versions and isolate target/cache ownership. Use existing operation-class permission policy so valid agent mode preparation does not require yolo.

## 受け入れ条件

- [ ] Cold and cache-hit setup produce equivalent ready-state.
- [ ] Preparation does not corrupt a sibling workspace target or cache.
- [ ] `ask` remains approval-gated and `agent` uses only the already granted scope.
- [ ] Failed preparation reports a retryable state rather than completed execution.

## テスト計画

Tool fixture tests, representative cold/hit benchmark, normal Rust gates and host tool-version gates.

## リスク

Do not move environment preparation into gh-git or expose arbitrary command argv.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260924-temote-development-harness-restructure.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
