# NR2–NR4: persist logical root identity and enforce all starts

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

NR2–NR4: persist logical root identity and enforce all starts. Parent: `issues/open/20260926-named-root-workspace-identity.md`.

## 背景

Dependency: NR1 reverse-resolution packet. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

A local cwd or persisted physical path can bypass or obscure the logical named-root boundary.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Persist root_name and root_relative_path separately from canonical host cwd; migrate legacy records without deleting them. Route local, direct HTTP, Fabric, restart, restore and upgrade entry points through one resolver. Keep configured TEMOTE_MCP_ROOTS support and provide actionable failures.

## 受け入れ条件

- [ ] New normal sessions always have a logical named root.
- [ ] Old records degrade safely.
- [ ] Outside-root or missing-root starts fail closed at every entry.
- [ ] Host root remapping does not rewrite live cwd.
- [ ] No remote root registration.

## テスト計画

Cross-entry matrix with overlapping roots, symlinks, missing targets, restart/upgrade and legacy metadata; Rust/gateway gates.

## リスク

Do not treat physical paths as cross-host identity.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260926-named-root-workspace-identity.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
