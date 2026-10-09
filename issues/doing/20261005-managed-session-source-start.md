# S3a: source XOR path session_start with durable provisioning

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

S3a: source XOR path session_start with durable provisioning. Parent: `issues/open/20260929-session-first-managed-provisioning.md`.

## 背景

Dependency: S0a conformance, RepositoryStore, workspace allocation. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

The public session_start currently accepts a named-root path and cannot provision a repository identity safely.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Extend authenticated session_start with mutually exclusive `source` and `path`. `source` managed form requires caller-supplied operation_id; persist Accepted before the first session-owned side effect and reconcile receipt/session/workspace after response loss. `path` remains explicit ExistingWorkspace compatibility. Keep remote host scope and no-yolo rule.

## 受け入れ条件

- [x] Both/neither source and path fail.
- [x] Managed missing operation_id fails before side effects.
- [ ] Same-key retries yield same SessionId/WorkspaceId/base.
- [x] Changed request conflicts.
- [ ] Concurrent resend does not duplicate.
- [x] Path behavior stays compatible.

## テスト計画

MCP schema/gateway contract tests, crash-point provisioning E2E, no-default and gateway tests, Rust gates.

## リスク

Never expose a raw arbitrary host path or promote public yolo.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Fresh post-repair canary selected visible default `gpt-6.1-sol` / `low`. A valid native failed report retained an initialized isolated bare store and failed marker after DNS/network approval failure. No workspace/ready marker or active generated session resulted. Model selection passed; source provisioning remains BLOCKED, and physical allocation/environment acceptance remains NOT RUN.

- 2026-10-06: Actual isolated managed-source canary reached a valid native BLOCKED report because child network approval for HTTPS fetch was rejected. No repository, workspace or readiness marker was created; provisioning did not activate. Fetch retry, physical allocation and downstream environment acceptance remain NOT RUN. Automatic model selection was corrected to preserve visible catalog default metadata; no approval boundary was widened.

- 2026-10-06: Source/path/UUID/replay conflict and pinned provisioning fixtures passed. Live managed source retry and concurrent fetch acceptance remain NOT RUN.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260929-session-first-managed-provisioning.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Source/path/UUID/replay conflict and pinned provisioning fixtures passed. Live managed source retry and concurrent fetch acceptance remain NOT RUN.
