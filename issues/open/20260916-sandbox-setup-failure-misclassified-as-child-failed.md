# sandbox setup failure が activity 上 ChildFailed に分類される

Status: open
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P2 observability / developer workflow friction
Related: `issues/doing/20260916-agent-mode-language-cache-writable-defaults.md`

## Observed in code review

`spawn_sandboxed_command_with_controls` は `run_session_command` の `Err` と、child processがnon-zeroで `render_output` が返す `Err` を同じ `JobActivityOutcome::Failed` に畳み、`finish_job_activity` は一律 `ActivityErrorKind::ChildFailed` を記録する。

そのため private cache directory作成失敗、sandbox policy/helper setup失敗などchild開始前のfailureも、activity surfaceではtest/build child failureと区別できない。

## Goal

child spawn前のsandbox/runtime setup failureと、実際に開始したchild processのnon-zero failureをtyped evidenceで区別する。error文字列パターンmatchには依存しない。

## Acceptance

- [ ] sandbox/runtime setup failureは `ChildFailed` 以外の適切なfixed activity error kindになる。
- [ ] child exit non-zeroは引き続き `ChildFailed`。
- [ ] cancellation / timeout semanticsを回帰させない。
- [ ] activity contract/gateway snapshotが必要なら同期する。
- [ ] focused tests + deterministic gate green。
