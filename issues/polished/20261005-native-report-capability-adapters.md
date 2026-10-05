# S2–S4: backend native report capability adapters

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

S2–S4: backend native report capability adapters. Parent: `issues/open/20260927-native-structured-output-agent-backends.md`.

## 背景

Dependency: S1 common report contract. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

Native structured-report support differs across Codex, OpenCode, Devin ACP and Devin Cloud.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Probe and record the exact supported native mechanism per backend/version; use S1 schema only when verified; retain existing compatibility profiles and final-message fallback for unsupported surfaces. Report report_source honestly, with bounded evidence and no wire change unless explicitly versioned.

## 受け入れ条件

- [ ] Capability table identifies supported/unsupported/unknown for all four backends.
- [ ] Supported path validates native schema.
- [ ] Unsupported path keeps compatibility behavior.
- [ ] Malformed native result is not misreported as valid.

## テスト計画

Versioned protocol fixtures per backend, provider live gate where available, report-schema regression and Rust gates.

## リスク

No fictitious provider feature or shared strict validator.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260927-native-structured-output-agent-backends.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
