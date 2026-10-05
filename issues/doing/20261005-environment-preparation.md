# D: scoped environment preparation adapters

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

D: scoped environment preparation adapters. Parent: `issues/open/20260924-temote-development-harness-restructure.md`.

## 背景

Dependency: Managed workspace allocation. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

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
- [x] Preparation does not corrupt a sibling workspace target or cache.
- [x] `ask` remains approval-gated and `agent` uses only the already granted scope.
- [x] Failed preparation reports a retryable state rather than completed execution.

## テスト計画

Tool fixture tests, representative cold/hit benchmark, normal Rust gates and host tool-version gates.

## リスク

Do not move environment preparation into gh-git or expose arbitrary command argv.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Fresh post-repair canary selected visible default `gpt-6.1-sol` / `low`. A valid native failed report retained an initialized isolated bare store and failed marker after DNS/network approval failure. No workspace/ready marker or active generated session resulted. Model selection passed; source provisioning remains BLOCKED, and physical allocation/environment acceptance remains NOT RUN.

- 2026-10-06: Actual isolated managed-source canary reached a valid native BLOCKED report because child network approval for HTTPS fetch was rejected. No repository, workspace or readiness marker was created; provisioning did not activate. Fetch retry, physical allocation and downstream environment acceptance remain NOT RUN. Automatic model selection was corrected to preserve visible catalog default metadata; no approval boundary was widened.

- 2026-10-06: Typed preparation/owner/cache/ready-marker fixtures passed. Live cold-versus-cache-hit setup remains NOT RUN.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260924-temote-development-harness-restructure.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Typed preparation/owner/cache/ready-marker fixtures passed. Live cold-versus-cache-hit setup remains NOT RUN.
