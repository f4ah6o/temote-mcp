# Events E1: catalog and durable subscription/outbox

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Events E1: catalog and durable subscription/outbox. Parent: `issues/open/20261005-fabric-mcp-events.md`.

## 背景

Dependency: Modern 2026-07-28 adapter path. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Fabric has no durable Events catalog or subscription lifecycle.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add job.state.changed and session.state.changed on modern authenticated MCP only; webhook delivery only, cursor null. Canonical subscription identity binds principal, callback, event and filters. Fabric stores subscription plus outbox. ttlMs omitted=24h, finite <=7d, null grants finite24h; finite refreshBefore. Rotation dual-signs for 5min.

## 受け入れ条件

- [x] Catalog schemas are stable.
- [x] Unsupported endpoints do not advertise events.
- [x] Equivalent key order refreshes one ID.
- [x] Restart retains subscription/outbox.
- [x] Revocation and expiry stop delivery.
- [x] No replay claim.

## テスト計画

Catalog/schema/canonicalization, TTL, rotation, restart and auth fixtures; gateway npm and Rust contract gates.

## リスク

Store signing keys encrypted/secret-safe; no log or tool output values.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: schema/subscription key canonicalization, transactional restart/outbox, revocation/finite expiry and no replay claim PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: fabric/src; fabric/test
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20261005-fabric-mcp-events.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.
