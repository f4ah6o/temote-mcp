# B/R: shared local task frontend and retry reconciliation

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

B/R: shared local task frontend and retry reconciliation. Parent: `issues/open/20260924-temote-development-harness-restructure.md`.

## 背景

Dependency: A1/A2 task identity and existing backend receipts. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

The MCP and local caller paths cannot yet prove one shared task lifecycle across disconnects and lost responses.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add typed task start/list/get/control to the versioned local control protocol and `temote-mcp task ... --local`; use the same orchestration core, full SessionInstance, permission class, receipt, and evidence boundaries as MCP. Carry caller-supplied operation_id across transports. Unknown side effects return reconciliation_required.

## 受け入れ条件

- [ ] A local start is rediscovered and controlled over MCP, and the reverse path works.
- [ ] Same-key replay returns one task.
- [ ] Changed request conflicts.
- [ ] Disconnect/restart does not fabricate success.
- [ ] Ask/agent permissions and bounded evidence match between frontends.

## テスト計画

Protocol unit tests; local↔MCP process-boundary E2E; restart/lost-response fixtures; normal Rust gates.

## リスク

Do not turn --local into yolo or a second authority.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20260924-temote-development-harness-restructure.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
