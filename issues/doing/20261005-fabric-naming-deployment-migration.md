# FBR2–FBR4: compatible Fabric command and deployment migration

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

FBR2–FBR4: compatible Fabric command and deployment migration. Parent: `issues/open/20260926-temote-fabric-product-boundary.md`.

## 背景

Dependency: FBR1 complete; integration PR after dependent phases. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Renaming live Gateway commands, routes and Durable Objects can strand hosts or state.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add forward-facing `temote fabric` commands while preserving gateway-agent, temote-mcp and TEMOTE_MCP_ compatibility; plan dual endpoint/Access/secret routing, health identity, DO/D1 migration and rollback. Rename gateway/ source only after runtime compatibility and deployment identity are verified. Use one integration PR; host updates and cf CLI deployment target temote.f12o.com are selected future execution steps.

## 受け入れ条件

- [x] Old clients and Host links continue through migration.
- [x] New names work.
- [x] DO/D1 state is preserved.
- [ ] Route/secret/Access rollback works.
- [x] No `No targets deployed` false success.
- [x] Source rename follows deployment proof.

## テスト計画

CLI/contract and migration fixture tests, gateway npm tests, Wrangler dry-run and read-only deployment preflight; live host/cf gate recorded separately.

## リスク

Never print secrets or destroy live DO/D1 state for naming.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Actual cf deployment, authenticated routing, canonical names and unchanged DO/D1 identities passed. Destructive rollback execution is NOT RUN; reviewed rollback/preflight fixtures passed.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260926-temote-fabric-product-boundary.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Actual cf deployment, authenticated routing, canonical names and unchanged DO/D1 identities passed. Destructive rollback execution is NOT RUN; reviewed rollback/preflight fixtures passed.
