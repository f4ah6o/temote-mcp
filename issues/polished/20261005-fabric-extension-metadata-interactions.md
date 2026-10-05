# Fabric extensions: optional metadata, mentions and elicitation

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Fabric extensions: optional metadata, mentions and elicitation. Parent: `issues/open/20261001-fabric-openai-mcp-extensions.md`.

## 背景

Dependency: PR #90 read-only MCP App baseline. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

The current adapter lacks optional extension metadata and bounded mention/interaction paths.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add capability-negotiated extension metadata to gateway/src/protocol.js and tests; keep modern and legacy standard MCP. Provide authorized bounded repository/Host/session mentions and provider-neutral interaction mapping to supported elicitation with standard fallback. Do not replace the protocol adapter with an SDK in this packet.

## 受け入れ条件

- [ ] Standard-only clients see unchanged core tools.
- [ ] Extension-capable clients receive metadata.
- [ ] Unauthorized mentions reveal nothing.
- [ ] Stale interactions fail safely.
- [ ] No approval bypass.
- [ ] Fingerprints change only intentionally.

## テスト計画

Protocol modern/legacy fixtures, routing/owner isolation and bounded disclosure tests, gateway npm test and Rust contract checks; client live gates NOT RUN until exercised.

## リスク

Keep the existing dashboard, Devin backend and Host authority unchanged.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20261001-fabric-openai-mcp-extensions.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
