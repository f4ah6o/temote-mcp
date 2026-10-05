# P2/P3: explicit agent prompt-hook capability adapters

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

P2/P3: explicit agent prompt-hook capability adapters. Parent: `issues/open/20261001-agent-prompt-observation-ingress.md`.

## 背景

Dependency: P0/P1 local durable ingress. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Direct Codex and Devin user turns may have no verified observable hook in the installed versions.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Probe actual Codex app-server and Devin ACP/Cloud supported surfaces. For each verified hook, emit only user_prompt_accepted/user_steer_accepted with local idempotent ingress. If no native hook is available, record coverage=unavailable or partial with reason; do not infer private turns from task metadata or fabricate integrations.

## 受け入れ条件

- [x] Supported hook produces one observation across retry.
- [x] Unsupported hook reports coverage gap.
- [x] System/developer/hidden prompts remain absent.
- [x] Task success unaffected by observer failure.

## テスト計画

Versioned capability fixtures and live opt-in provider probes; Rust gates; unavailable provider gates NOT RUN.

## リスク

Prompt contents use existing bounded content-reference policy and owner scope.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: direct user-message observation fixtures, replay identity, hidden-event rejection and capability gaps PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/codex_prompt_observer.rs; src/prompt_ingress.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20261001-agent-prompt-observation-ingress.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.
