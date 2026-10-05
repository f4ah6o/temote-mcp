# S1a/F: bare RepositoryStore ensure and pinned base

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

S1a/F: bare RepositoryStore ensure and pinned base. Parent: `issues/open/20260929-session-first-managed-provisioning.md`.

## 背景

Dependency: S0a conformance packet before wiring. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

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
- [ ] Changed input conflicts.
- [ ] Uncertain fetch requires reconciliation.
- [ ] A fresh managed store has no local main checkout.
- [ ] Dirty existing checkouts are untouched.

## テスト計画

Repository fixture tests for retry, crash, concurrent requests, stale fetch and invalid source; Rust gates; macOS/Linux host gates remain separately recorded.

## リスク

Do not fetch through an unvalidated source or mistake unknown freshness for current.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260929-session-first-managed-provisioning.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
