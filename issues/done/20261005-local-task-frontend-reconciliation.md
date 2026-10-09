# B/R: shared local task frontend and retry reconciliation

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

B/R: shared local task frontend and retry reconciliation. Parent: `issues/open/20260924-temote-development-harness-restructure.md`.

## 背景

Dependency: A1/A2 task identity and existing backend receipts. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

The MCP and local caller paths cannot yet prove one shared task lifecycle across disconnects and lost responses.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Add typed task start/list/get/control to the versioned local control protocol and `temote-mcp task ... --local`; use the same orchestration core, full SessionInstance, permission class, receipt, and evidence boundaries as MCP. Carry caller-supplied operation_id across transports. Unknown side effects return reconciliation_required.

## 受け入れ条件

- [x] A local start is rediscovered and controlled over MCP, and the reverse path works.
- [x] Same-key replay returns one task.
- [x] Changed request conflicts.
- [x] Disconnect/restart does not fabricate success.
- [x] Ask/agent permissions and bounded evidence match between frontends.

## テスト計画

Protocol unit tests; local↔MCP process-boundary E2E; restart/lost-response fixtures; normal Rust gates.

## リスク

Do not turn --local into yolo or a second authority.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: Initial local→MCP continuation failed CODEX_CONTINUATION_RUNTIME_OWNED and was repaired with supervisor-owned typed relay. Both directions, retained native reports, same-key replay, 1 MiB escaped changed-request conflict and owner-routed scoped evidence now PASS; disconnect/permission/full-instance fences are covered by regression fixtures.
- Scope: See the integration evaluation and corresponding implementation modules.
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20260924-temote-development-harness-restructure.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Initial local→MCP continuation failed CODEX_CONTINUATION_RUNTIME_OWNED and was repaired with supervisor-owned typed relay. Both directions, retained native reports, same-key replay, 1 MiB escaped changed-request conflict and owner-routed scoped evidence now PASS; disconnect/permission/full-instance fences are covered by regression fixtures.

### Final live acceptance (2026-10-06)

The isolated Codex 0.160.0 canary passed local→MCP and MCP→local control. Source task `c4e2d3fa-bb65-51a0-9dff-f345b501eff9` and successor `24c0632b-2354-5888-90ff-6f7fecac5797` share one thread and independent turns/receipts. Native reports were valid; verification remained not_run and delivery not_started. A 3,464-byte evidence record was read through the owning supervisor. Full Rust tests (125 library, 1,047 binary and ordinary integrations), Clippy, fmt, no-default check and diff gate passed. See the integration evaluation for the initial failure and repair.
- 2026-10-06: Completed after actual two-frontend native Codex acceptance and central regression gates.
