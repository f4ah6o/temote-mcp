# Keep completed task records within the storage limit when retaining raw results

Status: done
Model: unknown
Created: 2026-09-27
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Completed OpenCode and Devin ACP tasks retain a bounded raw final reply. Verify that adding it cannot make a valid task record exceed the 64 KiB persisted-record limit and lose the terminal update.

## 背景

The result-recovery implementation is recorded in [the malformed-report issue](../done/20260927-completed-task-malformed-final-report-json.md) and [the dogfood log](../../docs/dogfood-20260927.md). Independent OpenCode review found that the new 16 KiB `raw_result` shares a 64 KiB JSON record with report, usage, operation receipts, and operation tombstones. This is a static risk finding; no oversize failure has been reproduced.

## 問題

`TaskStore::save_locked` in `src/opencode_server.rs` and `src/devin_acp.rs` rejects serialized records larger than `MAX_TASK_RECORD_BYTES`. A terminal `apply_derived` update could fail if its record is already near that limit. The caller would then be unable to rely on durable raw-result recovery. Operation tombstones can also grow with repeated controls, so any fix must preserve replay protection.

## 目標

For every accepted task record, terminal status and the retained result are persisted within the storage bound, or a specific recoverable failure is returned without silently discarding the completed result. Truncation must be explicit, UTF-8 safe, and visible through task metadata.

## 対象外

Do not remove operation receipts or tombstones in a way that permits duplicate side effects. Do not expose raw result text inline in task views or widen the evidence boundary.

## 提案する方針

First construct maximal valid OpenCode and Devin ACP records using the current field bounds and operation history. Measure serialized size before and after adding raw output. If overflow is reachable, reserve a byte budget for mandatory record fields and persist a safely truncated raw reply or a separate bounded session-owned result record. Keep legacy record deserialization compatible.

## 受け入れ条件

- [x] A regression test covers a near-limit record, maximum raw result, and terminal save in both local-agent backends.
- [x] The test verifies durable reread, terminal status, `report_status`, raw-result availability, and explicit truncation.
- [x] Replay protection remains intact after repeated control operations and reconnect.
- [x] Scoped evidence remains the only public route for raw output.

## テスト計画

- Run focused OpenCode and Devin ACP record-size tests.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`.

## リスク

An implementation that merely drops old operation IDs could permit duplicate mutations. A separate result record needs the same instance ownership, retention, and path-safety rules as task records.

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Completed task result retention stays durable when task metadata approaches its record-size limit.

## 検証記録

- 2026-10-06: near-limit terminal raw-result save/reread, truncation, replay history and scoped evidence fixtures PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/opencode_server.rs; src/devin_acp.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

The dogfood review identified this edge case without a live failure. Preserve that distinction when prioritizing the issue.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.


## Polish decision (2026-10-01)

This is implementation-ready as a single bounded packet: first reproduce the size boundary with maximal valid records, then make the smallest persistence change only if overflow is reachable. Preserve operation replay protection and the scoped-evidence boundary. The issue records a static risk, not a reproduced production failure; tests must preserve that distinction.
