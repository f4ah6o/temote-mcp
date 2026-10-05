# Bound delegated-task waiting and avoid unchanged polling responses

Status: polished
Model: unknown
Created: 2026-09-27
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Add a bounded way for an MCP caller to wait for a delegated task's meaningful state change or terminal result without repeatedly sending identical `*_task_get` calls.

## 背景

[The 2026-09-27 dogfood run](../../docs/dogfood-20260927.md) used the real Temote MCP control plane. A candidate OpenCode review required 120 calls from start through evidence, and a Devin implementation task required 182 calls through typed interruption. Each active `task_get` advanced the revision even when the visible state remained `running`. The same run added `task_list` for ID rediscovery; it does not reduce polling calls.

## 問題

The client must choose a polling interval, carry `task_id` and `after_revision`, and repeatedly parse unchanged `running` responses. Revision churn prevents `not_modified` from reliably identifying a meaningful change. Long tasks consume many MCP calls and output tokens. A transient transport failure also forces the caller to reconstruct the same polling loop.

## 目標

One bounded read operation should wait for a meaningful task update or a timeout, return a compact unchanged result when nothing changed, and preserve the exact terminal state and scoped result evidence. The server must not start or duplicate a task while waiting.

## 対象外

Do not add executable command inputs, relax session ownership, inline transcripts, or make an unbounded blocking MCP call. Do not replace backend-specific control actions with a guessed common action.

## 提案する方針

After BW1 stabilizes semantic revisions, add optional `wait_ms` to each existing backend-specific `*_task_get`: omitted defaults to `0` (current nonblocking behavior), and accepted values are integers from `0` through `30000`. The read returns as soon as a meaningful revision or terminal state appears, otherwise a compact unchanged/timeout result at the deadline. Do not add a shared `task_wait` API. Bound backend probe cost, cancel on session stop or generation replacement, and preserve the existing `after_revision` and scoped-evidence contracts.

## 受け入れ条件

- [ ] With `wait_ms` omitted or `0`, each `*_task_get` retains its current nonblocking response contract; negative, noninteger, and values over `30000` are rejected.
- [ ] A live OpenCode and Devin task can be tracked to terminal with fewer MCP calls than repeated short-interval `task_get` polling, with measured counts.
- [ ] An unchanged wait returns a compact, explicit timeout/unchanged response without advancing a semantic revision.
- [ ] Waiting across a transient backend or transport failure cannot start or replay a task.
- [ ] A reconnect can use `task_list` plus the wait operation to resume tracking a retained task.
- [ ] Terminal results remain bounded and read through scoped evidence; failed, interrupted, and approval-wait states remain distinguishable.
- [ ] Session stop and full-instance replacement terminate or fence a pending wait safely.

## テスト計画

- Add deterministic default-zero, `30000` boundary, invalid-value, unchanged, terminal, transient-error, and session-replacement tests for each backend-specific API.
- Run the repository format, Rust test/clippy/no-default, gateway, and diff checks.
- Dogfood the candidate MCP server against the same long-running agent flow and record actual calls and response sizes.

## リスク

A server-side loop that polls a backend too often could reduce client calls without reducing load. A long MCP request may be cut off by client or proxy timeouts, so wait duration must be capped and reconnect behavior explicit.

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Delegated-task reads support bounded waiting and compact unchanged responses.

## 注記

This is measured P1 friction. Implement BW1 first. The API shape and numeric bound were selected on 2026-10-05; performance measurements remain to be run.
- 2026-10-05: The wait_ms contract is fixed; BW1 remains a prerequisite and implementation/tests remain pending.
