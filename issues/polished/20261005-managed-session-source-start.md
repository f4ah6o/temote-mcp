# S3a: source XOR path session_start with durable provisioning

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

S3a: source XOR path session_start with durable provisioning. Parent: `issues/open/20260929-session-first-managed-provisioning.md`.

## 背景

Dependency: S0a conformance, RepositoryStore, workspace allocation. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

The public session_start currently accepts a named-root path and cannot provision a repository identity safely.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Extend authenticated session_start with mutually exclusive `source` and `path`. `source` managed form requires caller-supplied operation_id; persist Accepted before the first session-owned side effect and reconcile receipt/session/workspace after response loss. `path` remains explicit ExistingWorkspace compatibility. Keep remote host scope and no-yolo rule.

## 受け入れ条件

- [ ] Both/neither source and path fail.
- [ ] Managed missing operation_id fails before side effects.
- [ ] Same-key retries yield same SessionId/WorkspaceId/base.
- [ ] Changed request conflicts.
- [ ] Concurrent resend does not duplicate.
- [ ] Path behavior stays compatible.

## テスト計画

MCP schema/gateway contract tests, crash-point provisioning E2E, no-default and gateway tests, Rust gates.

## リスク

Never expose a raw arbitrary host path or promote public yolo.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260929-session-first-managed-provisioning.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
