# Activity S01: イベント型と安全な summary を追加する

Status: done
Model: gpt-5.6-luna
Created: 2026-09-14
Updated: 2026-09-14
Branch: codex/20260914-activity-event-contract

## 概要

activity viewer の最初の作業として、イベント/update の型、安全な summary、サイズ検証、JSON の encode/decode を pure な Rust module に追加する。
想定する実装担当は `gpt-5.6-luna`、reasoning effort は `max` とし、既存の runtime や MCP handler は変更しない。

## 背景

親は [local activity viewer](../polished/20260914-local-activity-viewer.md) であり、本イシューはその S01 に対応する。
親には CLI、履歴、ライブ配送、承認、job の要件があるが、本イシューは通信や並行処理に依存しない型の実装だけを扱う。
以下に必要な契約を記載しているため、親の残りの作業を同時に実装する必要はない。

現行 [src/lib.rs](../../src/lib.rs) は sandbox を公開する library の入口である。
config/approvals/mcp/supervisor は [src/main.rs](../../src/main.rs) 側の binary module なので、library から直接参照できない。
既存 [Cargo.toml](../../Cargo.toml) に serde、serde_json、uuid があり、追加依存は不要である。

## 問題

既存 activity 通知は free-form な title/detail を受け付けるため、安全な保存用データとして転用できない。
新しいイベントを型で限定し、後続の履歴・通信実装が同じ JSON と上限を使えるようにする必要がある。

## 目標

- ソースコードの変更を src/lib.rs と、新規 src/activity.rs、src/activity/contract.rs の3つに収める。親子イシューの進捗・状態記録は別途更新する。
- 固定 enum と整数・UUIDだけから summary を生成できるようにする。
- clock、random UUID、session lookup を外部で決定して渡せる API とする。
- valid fixture と invalid input の単体テストだけで完了を確認できるようにする。

## 対象外

- ring buffer、broadcast、mutex、ActivityScope、Tokio task、socket、ACK、CLI、端末処理。
- 実際の session のロード、認可、approval、sandbox、job、Git、外部 integration。
- 既存 approvals::activity() の移行・変更。
- 親の全 operation enum の追加。ここでは後述の5種類だけを扱い、後続単位が残りを追加する。
- CHANGES.md、README、利用者向け docs、Cargo.toml/Cargo.lock、package version の変更。

## 提案する方針

### 変更するファイル

1. src/lib.rs に `pub mod activity;` を追加する。
2. 新規 src/activity.rs から `pub mod contract;` を公開する。まだ実装しない module を宣言しない。
3. 新規 src/activity/contract.rs に下記の型、checked API、同じ module 内の tests を追加する。

binary の main.rs には mod activity を追加しない。
後続の binary 実装は `temote_mcp::activity` を利用する。
config を library へ移動したり、dead_code の抑制や未実装の placeholder を追加したりしない。

### 定数と enum

| 名前 | 値・variant |
| --- | --- |
| ACTIVITY_SCHEMA_VERSION | 1 |
| MAX_ACTIVITY_SUMMARY_BYTES | 512 |
| MAX_ACTIVITY_EVENT_BYTES | 2048。event envelope の UTF-8 bytes、末尾 LF を除く。 |
| ActivityOperation | SessionStart、SessionStop、ReadFile、WriteFile、GitPull |
| ActivityState | Started、WaitingApproval、Running、Completed、Failed、Cancelled |
| ActivityErrorKind | InvalidInput、SandboxDenied、ApprovalDenied、RuntimeUnavailable、ChildFailed、ProtocolFailed、OperationFailed |
| ActivityRemote | Origin、Other |
| ActivitySummary | Empty、Failure { kind: ActivityErrorKind }、Git { remote: ActivityRemote } |

enum は snake_case の wire name を使う。
ActivitySummary から作る表示文字列は次の3形式だけとする。

- Empty → 空文字列
- Failure { kind } → `error=<kind の snake_case>`
- Git { remote } → `remote=origin` または `remote=other`

任意文字列、path、URL、argv、stdout/stderr、prompt を summary constructor の引数にしない。
単純な enum match で実装し、汎用 redaction framework を作らない。

### データ構造と API

以下の関数名と役割を既定とする。
private field と constructor の使い方は Rust の既存規約に合わせてよいが、checked API を通らずに wire data を生成できる公開 setter は作らない。

| 型・関数 | 入力と出力 |
| --- | --- |
| ActivityUpdate | schema_version、operation_id: UUID、operation、state、duration_ms: Option<u64>、summary: ActivitySummary。session/path/sequence/timestamp を持たない。 |
| EventStamp | sequence: u64、timestamp_ms: u64、session_id: Option<String>、session_instance: Option<UUID>。呼び出し側が値を渡す。 |
| ActivityEvent::from_update(update, stamp) | 検証済み ActivityEvent を返す Result。update の summary を安全な一行文字列に変換する。 |
| encode_update(&update) | 検証後に JSON bytes を返す Result。末尾 LF は付けない。 |
| decode_update(bytes) | サイズ検査 → strict JSON decode → schema/値検証の順で ActivityUpdate を返す Result。 |
| encode_event(&event) | `{"type":"activity","event":...}` の JSON bytes を返す Result。末尾 LF は付けない。 |
| ContractError | UnknownSchema、InvalidValue、InvalidSummary、TooLarge、InvalidJson の固定 enum。入力文字列や serde error の本文を保持・表示しない。 |

ActivityEvent の JSON fields は schema_version、sequence、operation_id、timestamp_ms、session_id、session_instance、operation、state、duration_ms、safe_summary とする。
optional field の None は省略せず null に serialize する。
ActivityUpdate の wire JSON は typed summary を保ち、safe_summary 文字列を入力として受け付けない。
summary は内部 tag `kind` の snake_case object とし、Empty は `{"kind":"empty"}`、Failure は衝突を避けて `{"kind":"failure","error":"approval_denied"}`、Git は `{"kind":"git","remote":"origin"}` とする。

エラーの対応は、JSON の構文・未知 field/enum・型・必須 key の違反を InvalidJson、schema の違いを UnknownSchema、値同士の不整合を InvalidValue、summary の長さ/control 違反を InvalidSummary、update/envelope の byte 上限超過を TooLarge とする。

検証条件：

- schema_version は1だけ。unknown enum、unknown field、型違い、必須 field 欠落は拒否する。optional な duration_ms も wire 上の key は必須とし、None は null を明示する。
- update の入力 bytes は2048以下とし、上限を超えた JSON を deserialize しない。
- sequence は1以上。timestamp_ms は0を許す。ID は uuid 型で扱い、この単位では自動生成しない。
- Started/WaitingApproval/Running の duration_ms は None。Completed/Failed/Cancelled は Some を必須とし、0は許す。
- session_id が None なら session_instance も None。session_id があって session_instance が None は、runtime 作成前の lifecycle event 用に許す。
- session_id は1〜64 bytes の ASCII 英数字・ハイフン・アンダースコア・ドットとし、単独の . と .. は拒否する。これは wire の値検証であり認可ではない。後段の runtime adapter は既存 config::validate_session_id() と instance 照合を引き続き使う。
- summary は512 UTF-8 bytes 以下、C0/C1・ESC・改行・CR・bidi control（U+061C、U+200E/F、U+202A〜E、U+2066〜9）を含まない。
- event は envelope 全体を serialize して2048 bytes 以下であることを確認する。文字数ではなく byte 数を数える。

summary の長さ/control character の内部 validator は個別にテストしてよいが、任意の text から ActivitySummary を作る公開経路にはしない。
S01 では event の decode、全体の状態遷移、duration の測定、sequence の採番、history を実装しない。

### golden fixture

以下と JSON value として一致する event を作れることを確認する。
key の出力順や空白を不要に固定しない。safe_summary の文字列は完全一致で確認する。

```json
{"type":"activity","event":{"schema_version":1,"sequence":1,"operation_id":"00000000-0000-4000-8000-000000000002","timestamp_ms":1780000000000,"session_id":"sf","session_instance":"00000000-0000-4000-8000-000000000003","operation":"git_pull","state":"completed","duration_ms":1832,"safe_summary":"remote=origin"}}
```

### 作業順序

1. module 宣言、定数、enum とその serde 表現を実装する。
2. update の checked encode/decode と固定 ContractError を実装する。
3. EventStamp と from_update、event envelope の encode を実装する。
4. 下記のテストを追加して実行し、ソースコードの差分が指定の3ファイルの範囲内であることを確認する。
5. 親の S01 に実際の API と検証結果を記録する。S02 以降は本イシューの完了条件に含めない。

## 受け入れ条件

- [x] 指定の3ファイルに pure module が追加され、既存 runtime/handler/permission の動作が変わらない。
- [x] 5 operation、6 state、固定 summary が指定の wire name に serialize される。
- [x] update round-trip と golden event が一致し、None は明示的な null になる。
- [x] unknown schema/field/enum、欠落 key、型違い、duration の state 不整合を固定エラーで拒否する。
- [x] summary の512 bytes境界、event/update の2048 bytes上限、session ID の値制約を検証する。
- [x] 不正入力中の secret sentinel が返却エラーの Display/Debug やログに出ない。
- [x] 更新した単体テストが1件以上実行され、通常 build と no-default-features build が通る。
- [x] 親に S01 の完了と実際の API を記録し、後段を未実装のまま明示する。

## テスト計画

`contract.rs` の `#[cfg(test)] mod tests` に次を追加する。
実サービス、supervisor、ネットワーク、sleep は不要である。

| ケース | 期待結果 |
| --- | --- |
| 各 state と duration の None/Some(0)/Some(1832) | 非 terminal は None だけ、terminal は Some だけを受理する。 |
| summary の3形式 | 空文字、error=approval_denied、remote=origin/other に一致する。 |
| 正常 update の encode/decode | 元の値と一致する。 |
| golden event | JSON value と safe_summary が一致する。 |
| session_id None/通常ID/空/. /.. /64 bytes/65 bytes | 指定した値制約と instance の整合に従う。 |
| update に session_id/path/safe_summary/未知 field を追加 | 拒否する。 |
| schema 2、unknown enum、欠落 key、string の duration、負の数 | 拒否する。 |
| 2048 bytes超の入力 | parse より前に TooLarge。境界内の不正 JSON は InvalidJson。 |
| summary validator に512/513 ASCII bytes、256/257個の é | 512 bytesまで受理し、それ以上を拒否する。 |
| summary validator に改行、CR、ESC、C1、bidi control | InvalidSummary。 |
| error 入力に SECRET_SENTINEL_ACTIVITY | error の Display/Debug に sentinel がない。 |
| 同一入力の再 encode | JSON value が同じ。clock/random の暗黙取得がない。 |

開発中の確認：

```sh
cargo fmt --all -- --check
cargo test --lib activity::contract
cargo check --no-default-features --all-targets
git diff --check
```

test filter が0件だった場合は検証成功とせず、module の登録を確認する。
commit 前には AGENTS.md の cargo test、cargo clippy --all-targets -- -D warnings も実行する。
本単位は gateway/shared transport を変更しないため、gateway npm test は親の統合段階で実施する。
実装後の検証では Rust の contract tests、通常の全 Rust tests、clippy、format、no-default-features build、diff check を実施した。S01 は gateway/shared transport を変更していないため gateway npm test は親の統合段階で実施する。

## リスク

- library から binary の config を参照すると build が崩れるため、この単位の型は自己完結させる。
- serde の Option は field 欠落を None にできるため、wire key 必須の検証を明示的に行う。
- public field、From<String>、raw error 保存を追加すると checked API を迂回できるため、構築・encode 経路を限定する。
- 未実装 enum の追加で範囲が膨らむため、S01 は5 operation に限定する。全 coverage は親の S14 までに満たす。

## 変更履歴

`CHANGES.md` impact: no

内部の pure contract だけを追加し、利用者が呼び出せる機能はまだ増えない。
親の S16 で viewer 全体の変更履歴を記載する。

## 注記

- 2026-09-14: 親の全体仕様から最初の独立実装単位を切り出した。対象モデルは gpt-5.6-luna、reasoning effort は max。Model 欄は文書の更新モデルを表し、正確な識別名を確認できないため unknown とする。
- 2026-09-14: 前提となる機能実装はなく、この文書だけで S01 に着手できる。並行処理や transport に関する質問は後段の責務であり、S01 の未解決事項ではない。
- 2026-09-14: S01 の変更範囲を3ソースファイルの pure contract に限定し、型・API・fixture・受け入れ条件を確定したため実装可能な polished とする。
- 2026-09-14: S01 の pure activity contract 実装と検証が完了したため、既存の done 状態へ移動する。
- 2026-09-14:
  S01: done
  Changed: src/lib.rs、src/activity.rs、src/activity/contract.rs
  Verified: `cargo test --lib activity::contract`（11 passed）、`cargo test --quiet`（55 library / 728 binary / integration tests passed）、`cargo clippy --all-targets -- -D warnings`、`cargo fmt --all -- --check`、`cargo check --no-default-features --all-targets`、`git diff --check`
  Interface for next step: `temote_mcp::activity::contract` の `ActivityOperation`、`ActivityState`、`ActivityErrorKind`、`ActivityRemote`、`ActivitySummary`、`ActivityUpdate::new`/`with_schema_version`、`EventStamp::new`、`ActivityEvent::from_update`、`encode_update`/`decode_update`、`encode_event`。update は typed summary、event は安全な `safe_summary` と null を保持する。
  Remaining: S02 以降の history、broker、scope、ingress、配送、CLI、runtime handler 接続は未実装。CHANGES.md、README、docs、Cargo metadata は S01 の対象外。
- 2026-09-14: S01 レビュー指摘の再検証を開始した。既存レイアウトに `doing/` がなく、`done` からの独自状態遷移は追加しない。operation/state の JSON 型、update と summary の重複 key、nullable field の存在確認を再検証する。
- 2026-09-14:
  S01 レビュー指摘: resolved。`ActivityOperation` / `ActivityState` の deserialize は指定された snake_case の JSON 文字列だけを受理し、object・array・number・bool・null と unknown name を `InvalidJson` に分類する。`ActivityUpdateWireVisitor` と `ActivitySummaryVisitor` は `MapAccess` で update のトップレベルおよび summary 内の重複 key（同値・異値とも）を拒否し、トップレベルを object に限定する。`duration_ms` は explicit null と key 必須を別に検査する。
  修正前の最小再現は raw JSON bytes で実施し、`cargo test --lib activity::contract::tests::review_reproduces` は 0 passed / 2 failed となり、enum object と重複 `schema_version` を受理する不具合を確認した。修正後は同じ2テストが 2 passed / 0 failed。duplicate fixture は `json!` / `Map` を使わず、重複 key を保持した bytes を使用した。
  nullable assertion は `get(...) == Some(&Value::Null)` に修正し、`update_round_trip_preserves_typed_summary_and_null_duration` と `preserves_explicit_nulls_in_event` で key の存在と null を同時に確認する。`rejects_missing_nullable_duration_key` は duration key 欠落を `InvalidJson` とし、`missing_event_nullable_key_fails_explicit_null_check` は event の3 nullable key を削除した fixture が null 検査を通らないことを確認する。
  Changed: `src/activity/contract.rs` と本子・親イシューの進捗記録。既存の S01 API は `temote_mcp::activity::contract` の `ActivityOperation`、`ActivityState`、`ActivityErrorKind`、`ActivityRemote`、`ActivitySummary`、`ActivityUpdate::new` / `with_schema_version`、`EventStamp::new`、`ActivityEvent::from_update`、`encode_update` / `decode_update`、`encode_event`。
  Tests: `review_reproduces_non_string_operation_and_state_acceptance`、`review_reproduces_duplicate_top_level_and_summary_keys`、`rejects_all_non_string_operation_and_state_values`、`rejects_non_object_updates`、`rejects_missing_nullable_duration_key`、`missing_event_nullable_key_fails_explicit_null_check` を追加・修正。`cargo test --lib activity::contract` は 17 passed / 0 failed / 44 filtered、`cargo test --lib activity::contract::tests::review_reproduces` は 2 passed / 0 failed / 59 filtered。
  Verified: `cargo test --quiet` は library 61、binary 728、SDK 3、integration 13（1+1+2+9）が passed、3 ignored、0 failed。`cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check` は全て exit 0。no-default-features の既存 dead-code warning は approvals/profile/session_control の binary 側で、S01 activity 由来ではない。issues CLI validate も violations なし。gateway の `npm test` は gateway/shared transport を変更していない S01 の対象外のため未実施。
  Interface for next step: update は typed summary を保持し、event は安全な `safe_summary` と nullable field の明示的 null を保持する。`decode_update` はサイズ検査、strict JSON、schema/値検証の順で処理する。
  Remaining: S02〜S16 の history、broker、scope、ingress、配送、CLI、runtime handler 接続は未実装。CHANGES.md、README、docs、Cargo metadata は変更していない。子イシューの状態は既存規約どおり `done` のままとする。
