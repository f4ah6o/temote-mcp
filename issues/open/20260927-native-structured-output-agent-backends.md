# Native structured output for Codex / OpenCode / Devin backends

## Status

open — implementation required

Polished child packet: `issues/polished/20260927-common-task-report-schema.md` (S1: backend-neutral report schema 単一ソース化, ready)。S2-S4 (Codex app-server / OpenCode serve / Devin ACP の native surface 実測+実装) は S1 の共通 contract に依存するため後続 packet とする。

Created: 2026-09-27 (Asia/Tokyo)

Related:
- `issues/open/20260922-agent-server-backends-cli-deprecation.md`
- `issues/done/20260923-devin-acp-backend.md`
- `issues/done/20260924-devin-cloud-backend.md`
- `issues/open/20260908-live-acceptance-matrix.md`
- `issues/done/20260927-completed-task-malformed-final-report-json.md` (fallback/retrieval guarantee; remains required even when native structured output is available)

## Problem

Temote の Codex / OpenCode / Devin 系 task backend は同じ bounded report contract を
持っているが、report の生成経路が揃っていない。

現状:

- `src/devin_cloud.rs` は session 作成時に `structured_output_schema` と
  `structured_output_required: true` を送り、structured output を第一候補として取得している。
- `src/opencode_server.rs` は prompt 内の `REPORT_INSTRUCTIONS` で JSON を要求し、
  最終 assistant message を Temote 側で parse / schema validation している。
- `src/devin_acp.rs` も最終 assistant message の JSON を Temote 側で検証する方式。
- `src/codex_app_server.rs` は app-server task contract 上で backend-native structured
  output を共通 report schema の必須経路として扱っていない。
- legacy `codex exec` には output schema を渡す実装実績がある。

prompt に「JSON だけ返す」と書くだけでは markdown fence、余分な prose、control
character、schema overrun など backend/model ごとの差が残る。各 backend が提供する
structured output / JSON Schema 機能を使い、Temote の report contract を wire level
でも固定したい。

## Goal

Codex、OpenCode、Devin の各 delegation backend で、利用可能な
**backend-native structured output** を使って同一の Temote report schema を要求する。

単に最終テキストを JSON parse できたことを「structured output 対応」と呼ばない。

## Scope

- Codex app-server: `src/codex_app_server.rs`
- OpenCode serve: `src/opencode_server.rs` + `unofficial-opencode-sdk`
- Devin ACP: `src/devin_acp.rs`
- Devin Cloud: `src/devin_cloud.rs`（既存 structured output 実装を共通化・回帰防止）
- legacy one-shot delegation は互換性確認対象だが、server backend を主対象とする

## Design requirements

### 1. report schema を single source of truth にする

現在 backend ごとに重複している report schema / validation 定義を backend-neutral な
場所へ寄せる。

最低限、既存 public contract の以下を変えないこと:

- `status`
- `summary`
- `base_commit`
- `changed_files`
- `checks`
- requested / observed model・effort の扱い
- report byte 上限
- schema validation / normalization rules

backend ごとに微妙に異なる schema を持たせない。

### 2. Codex app-server

現在サポート対象の app-server protocol を実測し、turn/request に JSON Schema /
structured output を指定できる正式なフィールドを使う。

要件:

- start / resume / steer で最終 report を要求する turn には同じ schema を送る
- protocol capability / response shape を fail-closed で検証する
- native structured payload を report の第一ソースにする
- schema を送ったことを fake app-server test で wire-level に固定する
- app-server が advertised / documented capability を持たないバージョンを
  「対応済み」と扱わない

### 3. OpenCode serve

現在の `PromptRequest` / serve API と `unofficial-opencode-sdk` を確認し、
JSON Schema / structured output 相当の native request surface がある場合は必ず使う。

SDK 側に型が欠けているだけなら SDK を先に拡張する。

要件:

- prompt text だけで JSON を強制する実装を primary path にしない
- schema/format が request に実際に載ることを fixture / fake server で検証する
- structured response を typed に取り出し、既存 bounded report validator に通す
- upstream API が本当に structured output を提供していない場合は、その exact
  version / capability / probe 結果を本 issue に記録し、prompt-only parse を
  「native structured output 完了」として close しない

### 4. Devin ACP / Devin Cloud

#### Devin ACP

ACP と現在の Devin CLI が structured output / schema constraint を expose しているか
live + protocol fixture で確認する。

利用可能なら:

- `session/prompt` の native structured output 経路へ共通 schema を渡す
- structured payload を第一ソースにする
- capability advertise が必要なら initialize/session capability として検証する

利用できない場合:

- exact CLI version / ACP capability / observed response を記録する
- prompt-only JSON は compatibility fallback と明示し、この項目を未達として残す

#### Devin Cloud

既存の:

- `structured_output_schema`
- `structured_output_required: true`

を共通 schema helper に載せ替え、挙動を維持する。

### 5. report source を区別する

内部 task/evidence では少なくとも以下を区別できるようにする:

- `native_structured_output`
- `final_message_compat`

public API を無理に変更する必要はないが、live acceptance で「どの経路を使ったか」を
確認可能にする。

native structured output が利用可能な backend/version で silent fallback して成功扱いにしない。

### 6. final-message fallback

移行期間の backward compatibility として fallback を残す場合でも:

1. native structured output を先に読む
2. native capability が無いと確認できた場合だけ final message fallback
3. fallback の利用理由を bounded metadata/evidence に残す
4. schema-invalid な native structured payload を final message で上書きして成功扱いにしない

## Acceptance criteria

- [ ] 共通 report JSON Schema が backend-neutral な 1 箇所から生成される
- [ ] Codex app-server request に native structured output schema が実際に送られる
- [ ] OpenCode serve request に upstream が提供する native structured output
      schema/format が実際に送られる。未提供なら exact blocker が実測で記録される
- [ ] Devin ACP で native structured output capability を使用する。未提供なら exact
      blocker が実測で記録される
- [ ] Devin Cloud は既存の structured output required behavior を維持する
- [ ] native structured payload が malformed / schema-invalid / oversized の場合に fail closed する
- [ ] native payload が壊れているとき final-message fallback で成功へ化けない
- [ ] start / resume / steer の各 report-producing path で schema 適用が抜けない
- [ ] fake transport / fixture tests が「schema を request に載せた」ことまで検証する
- [ ] Codex / OpenCode / Devin の backend parity test を追加する
- [ ] `cargo test --locked`、関連 feature matrix、gateway contract tests が PASS
- [ ] `docs/usage.md` / `docs/usage.ja.md` と必要なら
      `skills/temote-mcp/SKILL.md` を同期する
- [ ] live acceptance で backend ごとの `report_source` と structured-output capability を記録する

## Non-goals

- task ownership / lease / receipt / retention 契約の変更
- permission / approval policy の変更
- report public shape の不用意な破壊的変更
- model 自身の prose JSON compliance を structured output とみなすこと
- structured output 対応を理由に raw transcript / unbounded response を public evidence に露出すること
