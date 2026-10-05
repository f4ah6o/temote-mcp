# A: common backend dispatch and task contract

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

A: common backend dispatch and task contract. Parent: `issues/open/20260924-temote-development-harness-restructure.md`.

## 背景

The umbrella still lists common dispatch, task contract and policy/evidence ownership as unchecked.

## 問題

The parent phase is not fully implemented or verified.

## 目標

Complete this bounded phase with the parent safety contract intact.

## 対象外

Unrelated backends, unsupported integrations and a broader security model change are outside this packet.

## 提案する方針

Introduce backend-neutral typed status/start/get/control/list dispatch across Codex, OpenCode, Devin ACP and Devin Cloud. Keep backend-specific options/capabilities. Move shared permission/receipt/reconcile/evidence decisions into core while preserving MCP schemas, response JSON and gateway contract fingerprint unless explicitly versioned.

## 受け入れ条件

- [ ] All four backend entries flow through core.
- [ ] Existing tool schema and responses remain compatible.
- [ ] Ask/agent policy and evidence ownership retain behavior.
- [ ] Backend-specific unsupported action is explicit.

## テスト計画

Characterization snapshots, policy matrix, all backend fixtures, Rust and gateway contract gates.

## リスク

A refactor must not widen local or public yolo and must not inline child output.

## 変更履歴

Assess compatibility and user-facing impact when implemented; add a `CHANGES.md` entry if applicable. This issue preparation does not edit it.

## 注記

- 2026-10-05: Split from `issues/open/20260924-temote-development-harness-restructure.md`; no implementation or test PASS is claimed.
