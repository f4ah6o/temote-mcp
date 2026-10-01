# Bound delegated-task waiting and avoid unchanged polling responses

Status: open — measured P1 friction; BW1 semantic-revision prerequisite is polished
Model: gpt-6-sol
Created: 2026-09-27
Updated: 2026-09-27
Branch: feat/20260927-bounded-wait-for-delegated-tasks
Polished prerequisite: `issues/polished/20261001-task-get-semantic-revision-stability.md` (BW1, ready)

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

Measure semantic changes separately from each reconciliation attempt. Evaluate a bounded `wait_ms` on `*_task_get` against one shared `task_wait` operation, accounting for MCP transport timeouts and each backend's poll cost. Keep a cursor/revision contract and return retryable transport diagnostics without changing `operation_id` semantics for starts or controls.

## 受け入れ条件

- [ ] A live OpenCode and Devin task can be tracked to terminal with fewer MCP calls than repeated short-interval `task_get` polling, with measured counts.
- [ ] An unchanged wait returns a compact, explicit timeout/unchanged response without advancing a semantic revision.
- [ ] Waiting across a transient backend or transport failure cannot start or replay a task.
- [ ] A reconnect can use `task_list` plus the wait operation to resume tracking a retained task.
- [ ] Terminal results remain bounded and read through scoped evidence; failed, interrupted, and approval-wait states remain distinguishable.
- [ ] Session stop and full-instance replacement terminate or fence a pending wait safely.

## テスト計画

- Add deterministic long-running, unchanged, terminal, transient-error, and session-replacement tests for the chosen API.
- Run the repository format, Rust test/clippy/no-default, gateway, and diff checks.
- Dogfood the candidate MCP server against the same long-running agent flow and record actual calls and response sizes.

## リスク

A server-side loop that polls a backend too often could reduce client calls without reducing load. A long MCP request may be cut off by client or proxy timeouts, so wait duration must be capped and reconnect behavior explicit.

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Delegated-task reads support bounded waiting and compact unchanged responses.

## 注記

This is measured P1 friction. Implement BW1 first. The exact bounded-wait API shape remains intentionally open until the semantic cursor is stable and transport/backend behavior can be measured without revision churn.
