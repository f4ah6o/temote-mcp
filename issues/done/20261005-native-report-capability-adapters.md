# S2–S4: backend native report capability adapters

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

S2–S4: backend native report capability adapters. Parent: `issues/open/20260927-native-structured-output-agent-backends.md`.

## 背景

Dependency: S1 common report contract. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Native structured-report support differs across Codex, OpenCode, Devin ACP and Devin Cloud.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Probe and record the exact supported native mechanism per backend/version; use S1 schema only when verified; retain existing compatibility profiles and final-message fallback for unsupported surfaces. Report report_source honestly, with bounded evidence and no wire change unless explicitly versioned.

## 受け入れ条件

- [x] Capability table identifies supported/unsupported/unknown for all four backends.
- [x] Supported path validates native schema.
- [x] Unsupported path keeps compatibility behavior.
- [x] Malformed native result is not misreported as valid.

## テスト計画

Versioned protocol fixtures per backend, provider live gate where available, report-schema regression and Rust gates.

## リスク

No fictitious provider feature or shared strict validator.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: explicit four-backend capability table, native validation/malformed results and compatible fallback; one live Codex 0.160.0 native turn passed PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/report_contract.rs; docs/backend-capabilities.md
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260927-native-structured-output-agent-backends.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.
