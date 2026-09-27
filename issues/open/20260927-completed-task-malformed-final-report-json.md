# Completed task result must remain retrievable when final report JSON is malformed

Status: open — confirmed result-delivery contract gap  
Repository: `f4ah6o/temote-mcp`  
Created: 2026-09-27 (Asia/Tokyo)  
Related: `issues/open/20260927-native-structured-output-agent-backends.md`

Triage: keep this issue independent from native structured-output work. Native schemas reduce malformed reports; this issue owns the stronger fallback guarantee that an already-completed task never loses its bounded raw result when structured decoding fails.

## Summary

検証 task 自体は `completed` に到達しているのに、最終レポート JSON の形式不正だけを理由に結果本体が取得できないケースが再発した。

観測:

> 検証taskも completed に到達しました。ただし最終レポートJSONの形式だけまた不正だったため、結果本体が取得できません。

task execution の成否と final-report serialization / validation の成否を分離し、structured report の decode に失敗しても completed task の結果本体を失わないようにする。

## Problem

現状の結果配送経路では、少なくとも一部の task で以下の状態が成立しうる。

1. backend / agent 側の task execution は正常終了する
2. task state は `completed` に到達する
3. 最終レポートとして返された payload が invalid JSON または schema-invalid になる
4. report decoder / validator が失敗する
5. 呼び出し側から結果本体を取得できなくなる

これは execution failure ではなく report encoding / decoding failure である。

completed task の有用な出力が structured envelope の不正だけで不可視になると、検証結果・差分レビュー・テスト結果などを再取得するためだけに task をやり直す必要が生じる。

## Goal

task lifecycle state と final-report decode state を独立して扱い、task が `completed` なら、structured final report が壊れていても raw result / recoverable result を必ず取得できるようにする。

## Requirements

### 1. Preserve raw final output before structured decoding

final report を JSON / schema として解釈する前に、bounded な raw payload を保持する。

decode / validation failure が発生しても、その raw payload を捨てない。

### 2. Separate execution status from report status

少なくとも以下を別フィールド / 別概念として表現できるようにする。

- task / job state: e.g. `completed`
- final report state: e.g. `valid`, `invalid_json`, `invalid_schema`
- raw result availability
- structured result availability

final report が不正だからという理由で、実際に観測した `completed` を `failed` / `stopped` 等に読み替えない。

### 3. Fallback result on malformed report

structured report の decode に失敗した場合でも、呼び出し側が少なくとも次を取得できること。

- task / job identifier
- backend if known
- observed terminal state
- raw final output
- parse / schema validation error
- truncation information if bounded capture was applied

可能なら structured fallback envelope として返す。

### 4. Do not require rerunning a completed task

結果配送層の不具合だけを理由に、completed task を再実行しないと内容を取得できない設計にしない。

再取得 API / task detail / artifact retrieval のいずれかから同じ raw result を読めるようにする。

### 5. Keep bounded-output guarantees

raw fallback を導入しても、既存の output size / artifact size / retention の上限を外さない。

oversized output は明示的に truncated とし、silent loss にしない。

### 6. Actionable diagnostics

report decode failure は、少なくとも次を区別する。

- invalid JSON
- schema-invalid JSON
- oversized / truncated payload
- missing final report

エラーには task 実行そのものが completed であるかどうかも含める。

## Regression context

この failure class は過去にも OpenCode delegation の normalized final report で観測されている。

`docs/evaluations/codex-vs-opencode-live-20260912.md` では、非 trivial な回答で `invalid_json` / `invalid_report_schema` が発生し、最終レポートを強制・修復する follow-up が提案された。

その後 `issues/done/20260910-opencode-delegation-backend.md` では live recheck が 9/9 success まで改善したと記録されているため、今回の再発が同じ経路の regression なのか、別の task/result transport 経路なのかを切り分ける。

特定 backend に閉じた修正にせず、task result delivery contract 側で「completed result を失わない」保証を持たせる。

## Acceptance criteria

1. test fixture で task を `completed` にし、final report に intentionally malformed JSON を返す。
2. 呼び出し側は `completed` state をそのまま確認できる。
3. structured result の decode failure が明示される。
4. raw final output / result body を取得できる。
5. schema-invalid JSON のケースでも同様に raw result を取得できる。
6. missing report / malformed JSON / invalid schema / truncation を区別する regression test がある。
7. valid final report の既存 behavior と schema contract は変えない。
8. raw fallback は既存の bounded capture / retention 制約を守る。
9. completed task の report-decode failure を execution failure と誤分類しない。
10. 一度 completed になった task の結果を、再実行せずに再取得できる。

## Investigation notes

実装時には、task completion から user-visible result までの経路を追跡し、次を確認する。

- raw backend output を保持している層
- structured final report を抽出する層
- JSON parse / schema validation を行う層
- terminal task state を保存する層
- result detail / artifact retrieval API
- parse failure 時に raw payload が破棄される箇所

修正後は、実際の failure path と regression test の対応箇所をこの issue に記録する。
