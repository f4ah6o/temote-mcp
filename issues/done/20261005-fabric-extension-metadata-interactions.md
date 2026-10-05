# Fabric extensions: optional metadata, mentions and elicitation

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Fabric extensions: optional metadata, mentions and elicitation. Parent: `issues/open/20261001-fabric-openai-mcp-extensions.md`.

## 背景

Dependency: PR #90 read-only MCP App baseline. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

The current adapter lacks optional extension metadata and bounded mention/interaction paths.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add capability-negotiated extension metadata to gateway/src/protocol.js and tests; keep modern and legacy standard MCP. Provide authorized bounded repository/Host/session mentions and provider-neutral interaction mapping to supported elicitation with standard fallback. Do not replace the protocol adapter with an SDK in this packet.

## 受け入れ条件

- [x] Standard-only clients see unchanged core tools.
- [x] Extension-capable clients receive metadata.
- [x] Unauthorized mentions reveal nothing.
- [x] Stale interactions fail safely.
- [x] No approval bypass.
- [x] Fingerprints change only intentionally.

## テスト計画

Protocol modern/legacy fixtures, routing/owner isolation and bounded disclosure tests, gateway npm test and Rust contract checks; client live gates NOT RUN until exercised.

## リスク

Keep the existing dashboard, Devin backend and Host authority unchanged.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: capability negotiation, unchanged core fallback, owner-filtered mentions, stale interaction fences and fingerprints PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: fabric/src; fabric/test/extensions.test.mjs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20261001-fabric-openai-mcp-extensions.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.
