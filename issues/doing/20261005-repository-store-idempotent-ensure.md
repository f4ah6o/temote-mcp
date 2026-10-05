# S1a/F: bare RepositoryStore ensure and pinned base

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

S1a/F: bare RepositoryStore ensure and pinned base. Parent: `issues/open/20260929-session-first-managed-provisioning.md`.

## 背景

Dependency: S0a conformance packet before wiring. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Path-first setup does not provide an idempotent bare repository source or pinned freshness result.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add typed ensure_repository(source) under a host-configured store root; validate RepositoryId and owner scope; persist Accepted receipt before fetch/store creation; pin origin/main base for retries; never create or require local main. Explicit Git compatibility remains separate.

## 受け入れ条件

- [ ] Same operation_id and request returns one store and pinned base after response loss.
- [x] Changed input conflicts.
- [x] Uncertain fetch requires reconciliation.
- [ ] A fresh managed store has no local main checkout.
- [x] Dirty existing checkouts are untouched.

## テスト計画

Repository fixture tests for retry, crash, concurrent requests, stale fetch and invalid source; Rust gates; macOS/Linux host gates remain separately recorded.

## リスク

Do not fetch through an unvalidated source or mistake unknown freshness for current.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Fresh post-repair canary selected visible default `gpt-6.1-sol` / `low`. A valid native failed report retained an initialized isolated bare store and failed marker after DNS/network approval failure. No workspace/ready marker or active generated session resulted. Model selection passed; source provisioning remains BLOCKED, and physical allocation/environment acceptance remains NOT RUN.

- 2026-10-06: Actual isolated managed-source canary reached a valid native BLOCKED report because child network approval for HTTPS fetch was rejected. No repository, workspace or readiness marker was created; provisioning did not activate. Fetch retry, physical allocation and downstream environment acceptance remain NOT RUN. Automatic model selection was corrected to preserve visible catalog default metadata; no approval boundary was widened.

- 2026-10-06: Accepted repository claim/replay/concurrency and marker crash fixtures passed. Actual delegated fetch/store/base retry acceptance remains NOT RUN.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260929-session-first-managed-provisioning.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Accepted repository claim/replay/concurrency and marker crash fixtures passed. Actual delegated fetch/store/base retry acceptance remains NOT RUN.
