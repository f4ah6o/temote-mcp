# Server backends: credentialed parity and one-shot CLI cleanup

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Server backends: credentialed parity and one-shot CLI cleanup. Parent: `issues/open/20260922-agent-server-backends-cli-deprecation.md`.

## 背景

Codex app-server and OpenCode serve are implemented, while successful credentialed parity and legacy one-shot argv cleanup remain.

## 問題

The parent phase is not fully implemented or verified.

## 目標

Complete this bounded phase with the parent safety contract intact.

## 対象外

Unrelated backends, unsupported integrations and a broader security model change are outside this packet.

## 提案する方針

Run versioned side-by-side parity for structured report/usage/model/effort, denial, interrupt and orphan-free shutdown. After evidence, deprecate one-shot `codex exec` and `opencode run` integration entrypoints in stages, retaining the vendor binaries for app-server/serve. Keep current contract validation and explicit compatibility period.

## 受け入れ条件

- [x] Each parity dimension has a non-secret measured result or NOT RUN with reason.
- [x] Legacy path is removed only when no supported caller depends on it.
- [x] Server path retains receipts, scoped evidence and typed controls.
- [x] Migration docs describe behavior.

## テスト計画

Backend protocol fixtures and credentialed canaries, CLI compatibility tests, Rust gates; unavailable entitlement remains NOT RUN.

## リスク

Do not equate a working status probe with successful model turn parity.

## 変更履歴

Assess compatibility and user-facing impact when implemented; add a `CHANGES.md` entry if applicable. This issue preparation does not edit it.

## 検証記録

- 2026-10-06: all seven paired parity dimensions record fixture PASS separately from live NOT RUN; legacy entrypoints intentionally retained PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: docs/backend-capabilities.md; src/cli.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260922-agent-server-backends-cli-deprecation.md`; no implementation or test PASS is claimed.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.
