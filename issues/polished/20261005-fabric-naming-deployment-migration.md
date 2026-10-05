# FBR2–FBR4: compatible Fabric command and deployment migration

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

FBR2–FBR4: compatible Fabric command and deployment migration. Parent: `issues/open/20260926-temote-fabric-product-boundary.md`.

## 背景

Dependency: FBR1 complete; integration PR after dependent phases. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

Renaming live Gateway commands, routes and Durable Objects can strand hosts or state.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add forward-facing `temote fabric` commands while preserving gateway-agent, temote-mcp and TEMOTE_MCP_ compatibility; plan dual endpoint/Access/secret routing, health identity, DO/D1 migration and rollback. Rename gateway/ source only after runtime compatibility and deployment identity are verified. Use one integration PR; host updates and cf CLI deployment target temote.f12o.com are selected future execution steps.

## 受け入れ条件

- [ ] Old clients and Host links continue through migration.
- [ ] New names work.
- [ ] DO/D1 state is preserved.
- [ ] Route/secret/Access rollback works.
- [ ] No `No targets deployed` false success.
- [ ] Source rename follows deployment proof.

## テスト計画

CLI/contract and migration fixture tests, gateway npm tests, Wrangler dry-run and read-only deployment preflight; live host/cf gate recorded separately.

## リスク

Never print secrets or destroy live DO/D1 state for naming.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260926-temote-fabric-product-boundary.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
