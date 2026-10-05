# NR2–NR4: persist logical root identity and enforce all starts

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

NR2–NR4: persist logical root identity and enforce all starts. Parent: `issues/open/20260926-named-root-workspace-identity.md`.

## 背景

Dependency: NR1 reverse-resolution packet. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

A local cwd or persisted physical path can bypass or obscure the logical named-root boundary.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Persist root_name and root_relative_path separately from canonical host cwd; migrate legacy records without deleting them. Route local, direct HTTP, Fabric, restart, restore and upgrade entry points through one resolver. Keep configured TEMOTE_MCP_ROOTS support and provide actionable failures.

## 受け入れ条件

- [x] New normal sessions always have a logical named root.
- [x] Old records degrade safely.
- [x] Outside-root or missing-root starts fail closed at every entry.
- [x] Host root remapping does not rewrite live cwd.
- [x] No remote root registration.

## テスト計画

Cross-entry matrix with overlapping roots, symlinks, missing targets, restart/upgrade and legacy metadata; Rust/gateway gates.

## リスク

Do not treat physical paths as cross-host identity.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: all new normal entries fail closed without canonical logical roots; old scope and root-remap/replacement fences PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/named_roots.rs; src/supervisor.rs; src/config.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260926-named-root-workspace-identity.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.
