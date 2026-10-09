# OC2: bind OpenCode implementation task to scoped command workspace

Status: doing
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

OC2: bind OpenCode implementation task to scoped command workspace. Parent: `issues/open/20260927-opencode-checkout-command-execution-capability.md`.

## 背景

OC1 preflight only classifies blockers; a writable checkout and command capability still need an implementation contract.

## 問題

The parent phase is not fully implemented or verified.

## 目標

Complete this bounded phase with the parent safety contract intact.

## 対象外

Unrelated backends, unsupported integrations and a broader security model change are outside this packet.

## 提案する方針

Bind task to a caller-authorized managed WorkspaceId/canonical cwd from S2; expose only the backend command capability needed inside existing host sandbox/approval policy. Keep OpenCode V2 shell denied until a scoped policy and protected-state tests demonstrate safety. Reject incompatible repository/workspace requirements before task acceptance.

## 受け入れ条件

- [ ] A task with an authorized workspace can read/write that checkout and run allowed build/test commands.
- [x] Escape and protected-state access fail.
- [x] Absent workspace/capability returns a structured pre-acceptance blocker.
- [x] No raw argv or network policy is added to public tools.

## テスト計画

OpenCode serve fixture, checkout and command canary, symlink/escape/approval tests, normal Rust gates and live provider gate when available.

## リスク

Do not simply remove the existing shell deny rule.

## 変更履歴

Assess compatibility and user-facing impact when implemented; add a `CHANGES.md` entry if applicable. This issue preparation does not edit it.

## 検証記録

- 2026-10-06: Private registration on OpenCode 2.0.11 passed; scope/escape/pre-acceptance fixtures passed. Real provider read/write/build/test turn is NOT RUN.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260927-opencode-checkout-command-execution-capability.md`; no implementation or test PASS is claimed.
- 2026-10-06: Private registration on OpenCode 2.0.11 passed; scope/escape/pre-acceptance fixtures passed. Real provider read/write/build/test turn is NOT RUN.
