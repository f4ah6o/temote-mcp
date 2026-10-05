# Events E1: catalog and durable subscription/outbox

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Events E1: catalog and durable subscription/outbox. Parent: `issues/open/20261005-fabric-mcp-events.md`.

## 背景

Dependency: Modern 2026-07-28 adapter path. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

Fabric has no durable Events catalog or subscription lifecycle.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add job.state.changed and session.state.changed on modern authenticated MCP only; webhook delivery only, cursor null. Canonical subscription identity binds principal, callback, event and filters. Fabric stores subscription plus outbox. ttlMs omitted=24h, finite <=7d, null grants finite24h; finite refreshBefore. Rotation dual-signs for 5min.

## 受け入れ条件

- [ ] Catalog schemas are stable.
- [ ] Unsupported endpoints do not advertise events.
- [ ] Equivalent key order refreshes one ID.
- [ ] Restart retains subscription/outbox.
- [ ] Revocation and expiry stop delivery.
- [ ] No replay claim.

## テスト計画

Catalog/schema/canonicalization, TTL, rotation, restart and auth fixtures; gateway npm and Rust contract gates.

## リスク

Store signing keys encrypted/secret-safe; no log or tool output values.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20261005-fabric-mcp-events.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
