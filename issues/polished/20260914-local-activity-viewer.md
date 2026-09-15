# Temote MCP の activity を読み取り専用の local CLI で閲覧する

Status: polished
Model: gpt-5.6-sol
Created: 2026-09-14
Updated: 2026-09-15
Branch: codex/20260914-local-activity-viewer

## 概要

`temote-mcp activity [SESSION_ID] [--tail N] [--no-follow]` を追加する。
supervisor が安全な構造化イベントをメモリ内に有限件数保持し、同一ユーザーの CLI が直近の履歴と以後のイベントを表示する。
viewer は approval console と独立した読み取り専用の接続とし、接続・切断によって session、job、承認結果、permission mode を変更しない。

本イシューは通常優先度の機能実装をまとめる親イシューである。
CLI、内部通信、対象操作の計測、privacy の検証、利用者向け文書までを含める。
配送は best-effort であり、現在の全 job の稼働状態や監査履歴の完全性を保証する機能にはしない。

## 着手方法

実装担当の想定は `gpt-5.6-luna`、reasoning effort は `max` とする。
`Model: unknown` はこの文書を更新したモデルの記録であり、実装担当の指定とは別である。

最初に取り組むのは [S01: activity のイベント型と安全な summary](../done/20260914-activity-event-contract.md) だけとする。
この子イシューは通信・並行処理・既存 handler の変更を含まず、3ファイル以内で実装と単体検証を完了できる。
子イシューの本文だけで S01 を実装でき、親の全機能を同時に読み解く必要はない。
S01 完了後は下記の S02 以降を順番に進める。
機能全体の範囲は維持し、同時に解く設計判断を減らす。

- 1つの作業単位では、指定された責務・変更箇所・テストに集中する。後段を先回りして実装しない。
- 全体実装を依頼された場合は、単位ごとの検証を挟んで順に継続する。作業単位の区切りを架空の時間制限や実行枠として扱わない。
- 新規の内部型・関数名は以下を既定とする。既存コードとの衝突がある場合だけ名前を調整し、本文へ実際の対応を記録する。
- runtime ownership、approval policy、MCP response、公開 surface は既存契約を維持する。実装を簡単にするために境界を緩めない。
- 難所で失敗した場合は、その単位の最小 failing test、現在の観測、次に確認する1点を残す。既存設計の一括変更や根拠のない retry を避ける。
- 各単位の完了記録には変更ファイル、テスト名、実行結果、後段に渡す API を残す。全体の受け入れ条件は S16 まで未完了のままとする。

## 背景

2026-09-14 時点の実装を確認した結果、所有関係は次のとおりである。

| 既存箇所 | 確認できる責務と実装上の注意 |
| --- | --- |
| [src/cli.rs](../../src/cli.rs)、[src/main.rs](../../src/main.rs) | `noargs` による parser と dispatch。`session console` はあるが root command の `activity` はない。 |
| [src/session_control.rs](../../src/session_control.rs) | private Unix control socket、`AttachConsole`、`run_console()`、`handle_console_attachment()`、approval broker。newline-delimited JSON をサイズ制限付きで扱う。現在の `CONTROL_PROTOCOL_VERSION` は `2`。 |
| [src/supervisor.rs](../../src/supervisor.rs) | `SessionSupervisor` が session ごとの `RuntimeHandle`、restart context、lifecycle transition を所有する。 |
| [src/approvals.rs](../../src/approvals.rs) | `RuntimeHandle` は `JoinHandle` を持つ supervisor 内の非同期タスクであり、独立プロセスではない。session socket、approval、integration bridge、`ExpectedSessionInstance` の照合を実装する。 |
| [src/mcp.rs](../../src/mcp.rs) | `call_tool_with_local_agent_executable()` が MCP 操作を dispatch する。`Job`、`JobCompletion`、`jobs()` が MCP を処理するプロセス内で job を保持する。supervisor に既存の共通 job registry はない。 |
| [src/codex_app_server.rs](../../src/codex_app_server.rs) | `codex_task_*` の受付・制御と非同期 task を扱う。MCP call と Codex turn の完了は別である。 |
| [src/local_agent.rs](../../src/local_agent.rs)、[src/dev_tool.rs](../../src/dev_tool.rs) | agent/tool の検証済み分類を持つ。raw task、cwd、argv は新 summary へ渡さない。 |

`approvals::activity()` は `Message::Activity { title, detail }` を session socket へ一方向に送り、runtime の `show_activity_for_session()` が人間向け通知を出す既存経路である。
履歴・subscriber・replay は持たず、送信側の connect/write に専用 timeout もない。
`report_command_finished()` 等の通知には command、path、出力の抜粋が入り得るため、新 event stream の入力に流用できない。

関連文書・イシューは次のとおりであり、本機能の重複実装ではない。

- [docs/managed-sessions.ja.md](../../docs/managed-sessions.ja.md)、[英語版](../../docs/managed-sessions.md)：supervisor、console、runtime isolation、upgrade の運用契約。
- [20260825-http-supervisor-approval-console-reattach.md](../done/20260825-http-supervisor-approval-console-reattach.md)：approval attachment と runtime ownership の分離。
- [20260902-zero-downtime-supervisor-upgrade.md](../done/20260902-zero-downtime-supervisor-upgrade.md)：same-PID の coordinated restart/restore。live task を移送する仕組みではない。
- [20260911-default-agent-permission-mode.md](../done/20260911-default-agent-permission-mode.md)：既定の `agent` と中央集約された approval policy。
- [20260911-session-forget-stale-metadata.md](../done/20260911-session-forget-stale-metadata.md)：terminal metadata の削除。activity retention は結合しない。
- [20260908-08-codex-delegation-dogfood-and-app-server.md](../open/20260908-08-codex-delegation-dogfood-and-app-server.md)：Codex task の live acceptance。本機能の検証に実サービスへの依存を追加しない。

## 問題

利用者は常時 console を開いておかずに、どの session で操作が開始され、承認を待ち、完了・失敗・取消になったかを確認したい。
現状の approval console は承認専用であり、後から接続して最近の操作を確認できない。
free-form 通知をそのまま保存すると、credential、prompt、ファイル内容を新たに保持することになる。

開始イベントだけが残っていても、履歴の eviction、配送失敗、MCP プロセスの終了によって完了イベントが欠けた可能性がある。
viewer が「現在も running」と断定すると誤認を生むため、イベント履歴と現在状態の問い合わせを区別する必要がある。

## 目標

- 全 session または指定 session の直近 N 件を replay し、既定では live follow する。
- 通常の配送条件では操作の開始から終了までを同じ ID で追えるようにする。
- job ID を返した後も実行される操作は、worker の終了を観測する。
- 件数、byte 数、queue、subscriber 数、通信待ち時間を制限し、activity の配送待ちを本来の操作へ持ち込まない。
- credential や本文を保持しない型付き event contract とする。
- 既存の approval、sandbox、network、public/local、session ownership の契約を維持する。

## 対象外

- viewer からの承認、command 実行、job cancellation、session stop/restart、permission mutation。
- public MCP、HTTP route、gateway、host federation への activity API 公開。
- raw JSON 出力オプション、TUI、Web UI、永続ログ、SQLite、監査証跡、durable resume cursor。
- raw argv、stdout/stderr、ファイル本文、prompt、environment value、child tool 引数の保存。
- supervisor への job registry 移管、MCP/HTTP process crash 時の job lifecycle の再設計。
- Codex turn の全通知や token usage の stream 化。v1 の `codex_task_*` は後述のとおり MCP call の受付結果を扱う。
- permission mode、yolo、Git/network/path 境界の変更、未実装 MCP tool の追加。

## 提案する方針

以下は新規実装の契約であり、既存機能の説明ではない。

### CLI と終了条件

```sh
temote-mcp activity
temote-mcp activity sf
temote-mcp activity --tail 50
temote-mcp activity sf --tail 100 --no-follow
temote-mcp activity --tail 0
```

- `--tail` は十進整数 `0..=1024`、既定値 `100`。負数、小数、overflow、値欠落、未知 option、複数の session 引数はエラーとする。
- `SESSION_ID` は既存の `config::validate_session_id()` で検証する。引数なしは supervisor 全体、指定ありは完全一致で filter し、session-independent event は除外する。
- 有効な形式であれば停止済み・存在しない ID も filter として受け付ける。session の起動・probe・metadata 読み込みは不要とし、残存履歴がなければ空の replay を返す。follow 中に同名 session が作成された場合は表示する。
- filter 後の最新 N 件を `sequence` の昇順で表示する。`--tail 0` は replay なし、`--tail 0 --no-follow` は接続成功後すぐ正常終了する。
- stdout はイベント行、stderr は接続情報、空履歴、欠落、切断理由に使う。色付けは必須にせず、改行以外の terminal control sequence を出力しない。
- `--no-follow` は stdin に依存せず、replay 終端を受けて exit code `0`。終端前の EOF は `1`。
- follow は Ctrl-C、TTY stdin の EOF、socket EOF で viewer だけを終了する。stdin が pipe や `/dev/null` の場合は監視せず、起動直後の EOF で終了させない。
- Ctrl-C、正常な socket EOF、stdout の BrokenPipe は `0`、不正 frame・通信 timeout・接続不可は `1`。supervisor を自動起動せず、自動 retry もしない。切断理由と CLI 再実行で再接続できることを短く表示する。

表示は local timezone の時刻、session ID、session instance と operation ID の短縮表示、operation、state、terminal duration、safe summary を含む一行形式とする。
UUID の短縮表示は目視用であり、wire 上の同一性判定には完全な UUID を使う。
接続ヘッダーで「best-effort の最近のイベント。現在状態を保証しない」と示す。
job の現在状態を確認する手段は既存の `job_list` / `poll_job`、Codex task は `codex_task_get` とする。

### 型、時刻、privacy

新規 `src/activity.rs` を library 側の入口とし、`src/activity/contract.rs`、`history.rs`、`broker.rs`、`scope.rs`、`render.rs` を段階ごとに追加する。
[src/lib.rs](../../src/lib.rs) には `pub mod activity;` を登録する。
この層は config、session の読み込み、socket path、credential に依存させない。
binary 側は `temote_mcp::activity` を参照する。
config と送信先を扱う producer adapter は新規 `src/activity_runtime.rs` とし、既存 binary module と同様に src/main.rs へ `mod activity_runtime;` を登録する。
control transport は session_control.rs、runtime ingress は approvals.rs に置く。
library と binary に同じ activity source を二重登録しない。

| `ActivityEvent` field | 契約 |
| --- | --- |
| `schema_version` | `1`。未知の値は表示せず、その attachment をエラー終了する。 |
| `sequence` | broker が受理順に付ける `u64`。supervisor lifetime 内で `1` から単調増加する。overflow 時は新規 publish を停止し、wrap させない。 |
| `operation_id` | Temote が activity 用に生成する UUID。同じ操作の各 state で共有する。MCP request ID、checkpoint/Codex の caller-supplied operation ID は転用しない。 |
| `timestamp_ms` | broker が受理時に付与する Unix epoch milliseconds。wall clock が逆行しても順序は sequence で判定する。 |
| `session_id` | runtime ingress では接続先から設定する。supervisor 発行分は検証済み対象 ID、session-independent event だけ `null`。 |
| `session_instance` | supervisor が runtime 作成ごとに生成するメモリ内 UUID。同名 session の再作成で変わる。runtime 作成前の失敗や supervisor 自体の操作は `null` を許す。 |
| `operation` | 下記 coverage の固定 enum から canonical name へ serialize する。任意文字列を受け付けない。 |
| `state` | `started`、`waiting_approval`、`running`、`completed`、`failed`、`cancelled`。 |
| `duration_ms` | terminal では producer scope の `Instant` による開始からの経過時間を `u64` milliseconds に飽和変換する。それ以外は `null`。wall clock の差を使わない。 |
| `safe_summary` | 型付き constructor が作る一行文字列。UTF-8 で最大 `512` bytes。空文字列を許す。 |

producer update は typed summary enum を持ち、公開 event の free-form `safe_summary` を受け付ける API は作らない。
許可する値は固定 enum（agent、access mode、permission mode、result/error classification）、上限付き整数（件数、task bytes、exit code）、Temote が生成した job UUID に限定する。
Git remote は任意名称を表示せず、`origin` / `other` の固定分類だけにする。
session ID は既存 routing identity として表示する明示的な例外であり、構文検証を秘密判定と同一視しない。

次の値は summary の入力にも、ring/broadcast/diagnostic の内容にも入れない。

- credential、token、API key、cookie、Authorization header、1Password value/locator、`op://` の参照文字列。
- command、stdout/stderr、agent prompt、MCP payload、environment value、approval detail。
- cwd、ファイル path、Git branch、任意 remote 名、URL、query、body、child tool/resource の任意名称・引数値。

raw error を stringify してから redact する設計は採らない。
失敗点で `invalid_input`、`sandbox_denied`、`approval_denied`、`runtime_unavailable`、`child_failed`、`protocol_failed`、`operation_failed` など固定 enum へ変換する。
識別できない原因は `operation_failed` とし、`anyhow` の message を解析して分類しない。
既存 API が deny boolean しか返さない経路では、console 不在を推測せず `approval_denied` とする。
型・size の検証後にも、renderer は C0/C1、ESC、改行、CR、bidi control の注入を拒否または可視化する。

### coverage と発行責任

下表を機能全体の完了時に満たす v1 の必須 coverage とする。
S01 では最小の operation enum だけを実装し、後段でこの表を満たすまで追加する。中間段階を完全な viewer としてリリースしない。
対象/除外を分類する一覧をコードにも置き、新しい tool を追加したときの判断漏れを検出する。
実装途中の silent な省略は受け入れ条件を満たさない。

| 対象 | 発行責任と完了の意味 |
| --- | --- |
| `session_start`、`session_stop`、`session_restart`、`session_permission_mode`、`session_permission_allow`、`session_permission_revoke`、`session_restart_policy`、`session_forget` | supervisor の lifecycle/permission 処理。MCP dispatch から重複発行しない。restart の内部 stop/start は外側の restart に含める。 |
| `session_crash`、`session_auto_restart`、`supervisor_upgrade` | supervisor の監視・復旧・upgrade 処理。crash は検出時に開始し直ちに failed（duration 0）。成功した exec は旧 broker の終了なので、旧 generation に upgrade 完了イベントを捏造しない。 |
| `execute`、`start_command`、`local_agent_run`、`dev_tool_run` | MCP 受付時に scope を作り worker に引き渡す。job ID 返却で完了せず、worker の結果・停止・lifetime timeout を terminal とする。 |
| `poll_job`、`job_list`、`stop_job`、local-only `without_sandbox` | MCP call 自体の結果。poll の成功と対象 job の成功を区別する。stop_job は対象 job の取消と自身の呼び出し完了を別 ID で表す。 |
| `git_add`、`git_commit`、`git_fetch`、`git_pull`、`git_push` | MCP call の結果。出力本文を扱わず、終了値と固定分類だけを渡す。 |
| `read_file`、`get_image`、`list_directory`、`evidence_read`、`write_file`、`apply_patch`、`checkpoint_save`、`checkpoint_load`、`work_handoff` | MCP call の結果。読み取り結果、checkpoint 本文、patch 内容は含めない。 |
| `friction_summary`、`learning_candidate_list`、`recall`、`recall_feedback` | MCP call の結果。結果本文や query は含めない。 |
| `onepassword_mcp_discover`、`onepassword_mcp_read_resource`、`onepassword_mcp_call`、`onepassword_item_get`、`onepassword_secret_resolve`、`onepassword_service_account_status`、`onepassword_service_account_run` | 外側の MCP call だけを発行し、内部の child call を二重に記録しない。 |
| `kintone_mcp_status`、`kintone_mcp_discover`、`kintone_mcp_call`、`kintone_cli_status`、`kintone_cli_run` | 外側の MCP call の結果。 |
| `codex_status`、`codex_task_start`、`codex_task_get`、`codex_task_control` | MCP call の完了。start/control の成功は固定 summary `result=accepted` で受付成功を示す。Codex turn の完了ではない。 |

`initialize`、`ping`、`tools/list` 等の protocol request、`session_list`、`session_info`、viewer の attach/detach は除外する。
session をロードできない request、unknown tool、不正 session ID、public 側で拒否される yolo/`without_sandbox` も runtime に紐付けて発行しない。
session-independent な OAuth/client authorization approval と Codex child 内部の approval 通知は v1 の個別イベント対象外とし、その認可処理も変更しない。
remote host の event はこの host の履歴へ集約しない。

scope の状態遷移は次のとおりとする。

1. 既知の対象 tool と許可された session を確定した後、operation 固有の引数検証前に started を発行する。supervisor 操作は対象 ID の形式検証後を開始点とする。
2. 中央の approval policy が実際に local approval を要求する場合、request 送信直前に同じ scope から waiting_approval を発行する。approval 応答待ちを意味し、console への配送成功を意味しない。
3. 検証・承認を通り実処理を開始したときに running を発行する。短い読み取りは started -> completed/failed を許す。agent で prompt を省略する操作に架空の waiting/allow を作らない。
4. 正常な処理結果は completed、失敗は failed。Temote による明示停止、session stop の検出、job lifetime timeout は cancelled とし、理由は `stop_requested`、`session_stopped`、`timeout` とする。child 自体のエラーは failed。
5. 追加の local approval が必要な操作は running -> waiting_approval -> running を許す。terminal は競合しても1回だけ発行を試みる。

`ActivityScope` または同等の型に ID、開始 Instant、terminal 済みフラグを持たせ、approval helper と worker へ明示的に伝える。
同じ scope の state 更新と queue 投入を直列化し、terminal 確定後の waiting/running を抑止する。
Tokio の task-local が spawn を越えて自動継承される前提にはしない。
`Job` と worker で同じ scope を共有し、stop_job と自然完了の競合は既存の完了結果の確定箇所に合わせて解決する。
`JoinHandle::abort()` で terminal が消えないよう取消の確定箇所を計測するが、既存の return value や job cleanup 契約を変えない。
MCP process の強制終了や runtime 消失後の配送失敗では terminal が欠け得る。未受信の成功・取消を viewer 側で補完しない。

### runtime ingress と非同期配送

MCP プロセスからは既存 session Unix socket に新しい `Message::ActivityUpdate` を送る。
update の schema_version は1を必須とし、未知 schema/field は固定 classification で拒否する。parse error に入力内容を添えてログ出力しない。
update に任意 session ID、path、sequence、timestamp を設定させず、開始時に得た session の started_at / process_id を expected instance として必須で持たせる。
受信 runtime は自身の ID と合わせて既存 `ExpectedSessionInstance` と同等の一致検証を行い、正規の session identity と supervisor 発行の session_instance を付与する。
現行 runtime をロードし直して古い scope を新しい instance へ付け替えない。

supervisor は runtime 作成時に instance に束縛した activity sink を渡す。
停止・置換した sink を無効にし、旧 runtime/producer の update が新しい同名 session に混ざらないようにする。
runtime は supervisor と同じプロセス内なので、ここに追加の process IPC は不要である。
新 message は upgrade quiesce 中も観測通知として扱い、処理中の操作数や approval queue に登録しない。
retired sink への通知は破棄する。

MCP の計測点は bounded queue に `try_send` するだけとし、connect/write や subscriber の読み取りを await しない。
プロセスごとに1つの bounded sender worker を持たせ、無制限の spawn、session ごとの無制限 queue、retry queue を作らない。
worker は1件ずつ送信し、runtime が broker への受理/破棄を決めた後に返す固定の短い ACK を待ってから次へ進む。
通常時の同一 producer 内の順序を保つためであり、MCP operation が ACK を待つ構造にはしない。
connect/write/ACK 全体に1秒の deadline を設け、失敗時はその update を捨てる。
runtime 側の ACK 送信も bounded な connection task と write deadline で処理し、runtime loop で遅い peer の読み取りを待たない。ACK の失敗は当該 connection だけを終了する。

queue full、broker の lock 競合、shutdown、通信失敗では操作の return value・承認応答を変えない。
broker が受理する前の欠落には sequence がなく、後述の gap marker で正確な件数を通知できないことを docs に明記する。
「操作を block しない」と「全操作を必ず記録する」を同時に要求しない。
旧 Message::Activity とその出力は互換経路として残し、新経路への adapter 入力にはしない。

### broker、retention、replay/live 境界

SessionSupervisor が1つの ActivityBroker を所有する。
broker は ring、live broadcast、sequence/generation、eviction flag など固定サイズの管理情報だけを保持する。
全 operation の状態 map や session 別の無制限履歴は作らない。

| 制限 | v1 の値 |
| --- | --- |
| ring event 数 | 4096 |
| serialized event envelope | 2048 bytes 以下（末尾 LF を除く） |
| ring の serialized bytes 合計 | 8 MiB 以下 |
| summary | 512 UTF-8 bytes 以下 |
| replay tail | 0..=1024 |
| live broadcast capacity | 1024 events |
| MCP producer queue | 256 typed updates。各 update は event と同等の byte 上限を事前検証する。 |
| activity attachments | supervisor 全体で同時16。超過は固定エラー `activity_busy`。 |
| control 初回 read / CLI の connect・attach 応答待ち | 5秒 |
| server の frame write timeout | frame ごとに5秒 |
| CLI stdout queue / write timeout | 最大256行、行ごとに5秒。飽和または timeout で当該 viewer を1で終了する。 |

ring は count と合計 bytes を両方検査し、必要な分だけ最古の event を evict する。
event は共有参照にできるが、queue、snapshot、subscriber を含めて上限を保つ。
publish は短い lock を try_lock で取得し、sequence 付与、ring 更新、broadcast の非待機 send を同じ critical section で行う。
viewer の snapshot コピーもこの lock 内で行うが、filter 後の最大1024件に制限し、通信は lock の外で行う。

attachment は subscriber 登録、snapshot 終端 sequence、filtered tail、ring の eviction flag を同じ critical section で取得する。
その後 replay を昇順に送り、終端 marker を送り、follow では snapshot 終端より後の event だけを送る。
通常の配送条件では、この切り替えで重複・取りこぼしを生じさせない。
viewer 不在でも ring は更新する。

`history_truncated` は「この supervisor generation で ring eviction が一度でも発生した」という global な保守的フラグとする。
指定 session の履歴が失われた件数や、tail による意図的な件数制限を意味しない。
要求件数未満でも、単にイベントが少ない場合は truncation としない。
follow の gap は broadcast lag が実際に検出されたときだけ出す。
filter で除外した event による sequence の飛びを gap と誤認しないよう、受信側は filter 前に global cursor を進める。
欠落区間は全 session に対するものと明記し、指定 session の欠落件数とは断定しない。

### control protocol と互換性

AttachConsole と独立した request/handler/client を追加する。
request は次の field だけを許し、未知 field は拒否する。

```json
{"command":"attach_activity","schema_version":1,"session_id":null,"tail":100,"follow":true}
```

`handle_control_connection()` は `handle_activity_attachment()` に直接分岐する。
`dispatch_request()` は先頭で reap_finished() を呼ぶため、新 attachment と互換性確認をこの branch に流さない。
approval registration と allow response の処理も共有しない。

成功時には既存 ControlResponse 形式の attach 応答を返し、その後は専用 envelope を送る。
以下の UUID/sequence は wire fixture の例である。

```json
{"ok":true,"result":{"control_protocol":2,"activity_schema":1,"generation":"00000000-0000-4000-8000-000000000001","snapshot_sequence":812,"replayed":1,"history_truncated":false},"error":null}
{"type":"activity","event":{"schema_version":1,"sequence":812,"operation_id":"00000000-0000-4000-8000-000000000002","timestamp_ms":1780000000000,"session_id":"sf","session_instance":"00000000-0000-4000-8000-000000000003","operation":"git_pull","state":"completed","duration_ms":1832,"safe_summary":"remote=origin"}}
{"type":"activity_end","snapshot_sequence":812,"history_truncated":false}
{"type":"activity_gap","scope":"all_sessions","after_sequence":820,"through_sequence":823,"dropped":3}
```

activity_end は replay の終端であり、follow の場合も1回送る。
generation は supervisor 起動時に新規生成するメモリ内 UUID とし、空履歴の snapshot_sequence は0とする。
gap の区間は (after_sequence, through_sequence] で、dropped はその区間内の未配送 global events 数とする。
snapshot 終端以前の event を二重に gap として数えない。

本変更は追加 request と activity schema の追加であり、既存 request/response を変えない限り CONTROL_PROTOCOL_VERSION = 2 を維持する。
新 CLI は attach 応答の protocol/schema を検証してから表示する。
旧 supervisor が未知 request を閉じる場合も、初回 response deadline 内に非対応エラーで終了し、黙って follow しない。
新 supervisor と旧 CLI の既存操作、protocol 2 の upgrade capability check / restore-plan 契約を維持する。
旧 MCP producer の free-form 通知を構造化イベントと誤認せず、完全な v1 coverage の検証には更新済み producer を使用する。

activity handler では request 後の入力は EOF による切断検知だけに使い、追加 byte が来たらその attachment を終了する。
allow、stop request、別の control command を同じ connection 上で dispatch しない。
CLI は follow 中に write half を保持し、初回 request 後に shutdown() する既存 one-shot client をそのまま流用しない。
server は後続入力を蓄積せず、writer timeout や malformed input は当該 viewer だけに閉じ込める。
CLI の stdout 書き込みは signal/socket 処理を無期限に塞がない方式にする。特にキャンセルできない blocking write task の終了を無期限に待たない。
no-follow は全 replay 行の出力完了後に正常終了し、取消時の出力 drain は最大5秒とする。

### security と再接続

既存 socket directory の0700、socket の0600、path validation、bounded line reader を維持する。
host_id を認証の代替にせず、same-user の private local socket だけで提供する。
activity request に command、path、credential、MCP payload を受け付けない。
public tools/list、HTTP route、gateway routed-tool contract、gateway agent protocol に追加しない。
認証済み public client が実行した local session 操作は local viewer の対象になるが、その逆方向に閲覧 API は作らない。

Ctrl-C、EOF、端末終了、slow viewer、複数 viewer のうち1つの切断は subscriber だけを破棄する。
approval denial、session stop、job cancellation、permission change を誘発しない。
socket 切断を伴う supervisor restart/upgrade では follow が終了する。
再実行時は新しい generation の tail を取得する。旧 generation の sequence を再開 cursor にしない。

event、broker generation、session instance UUID は disk、session metadata、upgrade restore plan、friction/evidence store に保存しない。
同じ supervisor 内の session restart では古い履歴を残し、新 instance のイベントと識別して表示する。
supervisor の exec/restart/crash では history は消える。
upgrade で runtime が再作成されることや、通常の supervisor restart が pending restart context を自動復元しないことは既存 lifecycle contract に従う。

### S01〜S16 の作業単位

依存が完了した単位から番号順に実施する。
次の表の「確認」は開発中の最小確認であり、commit 前の AGENTS.md の gate を省略する指定ではない。
各単位が終わったらチェックを更新する。途中の未実装分を完成扱いにしない。

| 完了 | 単位・前提 | 主な変更箇所とこの単位だけの成果 | 確認・完了条件 |
| --- | --- | --- | --- |
| [x] | S01 / 前提なし | [子イシュー](../done/20260914-activity-event-contract.md)。lib.rs、activity.rs、activity/contract.rs に pure な event/update/summary 型と checked encode/decode を追加する。 | `cargo test --lib activity::contract`。17 tests passed。型の golden/invalid input tests とレビュー回帰（strict string enum、duplicate key、nullable key presence）が通り、socket、clock の自動取得、runtime 変更なし。 |
| [x] | S02 / S01 | activity/history.rs に `ActivityHistory`、count/byte eviction、filtered tail、truncation flag を追加する。完成した event と既知の serialized size を入力にする。 | `cargo test --lib activity::history`。8 tests passed。容量3に1〜4を入れると2〜4、複数件 eviction、byte 上限、filter 後の tail、空履歴、0件、timestamp 逆行、truncation flag、read-only、1025件超の履歴で返却対象だけを clone する上限を確認。まだ lock/broadcast を使わない。 |
| [x] | S03 / S02 | activity/broker.rs に `ActivityBroker`、sequence/generation、atomic snapshot+subscribe、global gap 計算を追加する。clock/UUID は constructor から注入可能にする。 | `cargo test --lib activity::broker`。後述の境界 fixture、no subscriber、capacity、16 attachments、sequence overflow。socket は作らない。 |
| [x] | S04 / S01 | activity/scope.rs に `ActivityScope` と同期の `ActivityEmitter::try_emit(update)` を追加する。ID、Instant、状態遷移、terminal 一回性だけを扱う。 | `cargo test --lib activity::scope`。recording emitter で正常、deny、繰り返し waiting、finish 競合、emitter full を確認。実際の approval/job へは接続しない。 |
| [x] | S05 / S03 | supervisor.rs と approvals.rs に instance-bound sink の生成・無効化と typed ingress を追加する。既存 session ID + started_at/process_id の照合を使う。 | `cargo test --bin temote-mcp activity_ingress`。7 tests passed。直接 socket fixture で正常/stale/retired/oversized/追加 field/ACK failure と旧 variant の互換性を確認。MCP dispatcher は変更していない。 |
| [x] | S06 / S04,S05 | activity_runtime.rs と main.rs にプロセス単位の bounded producer と `ActivityEmitter` adapter を追加する。送信先と expected instance は内部で config から得る。 | `cargo test --bin temote-mcp activity_producer`。5 tests passed。受信 fixture で順序、queue full、1秒 deadline、no retry、raw error 非表示を確認。実操作の計測はまだ追加していない。 |
| [x] | S07 / S03,S05 | session_control.rs に AttachActivity と独立 handler を追加し、supervisor 内の broker に接続する。 | `cargo test --bin temote-mcp activity_attachment`。9 tests passed。wire fixture で replay/end/follow/gap、未知 field、後続 allow/stop、old peer、timeout を確認。CLI parser は変更していない。 |
| [ ] | S08 / S01,S07 | activity/render.rs の pure renderer と session_control.rs の finite replay client を作る。内部 test client から no-follow を検証する。 | `cargo test --lib activity::render` と `cargo test --bin temote-mcp activity_client`。固定時刻文字列、UUID短縮、control character、end 前 EOF。まだ未完成の CLI を公開しない。 |
| [ ] | S09 / S08 | CLI の follow/output adapter、cli.rs、main.rs の activity dispatch を追加する。 | `cargo test --bin temote-mcp activity_cli`。tail parser、TTY/non-TTY、BrokenPipe、Ctrl-C、5秒出力待ち。fixture broker で既定 follow と no-follow が動く。 |
| [ ] | S10 / S06,S09 | mcp.rs の read_file/write_file、supervisor の session_start/session_stop に scope を接続し、最初の実操作 end-to-end 経路を完成させる。 | `cargo test --test activity_e2e activity_file_roundtrip`。一時 supervisor と2 sessionで read/write、filter、viewer 切断後の生存を確認する。 |
| [ ] | S11 / S10 | approval helper の互換 wrapper と git_* の計測だけを追加する。 | `cargo test --bin temote-mcp activity_approval`。ask の allow/deny、不在 console、agent の skip。local bare remote で git_pull の state 順序と既存結果を確認。 |
| [ ] | S12 / S11 | mcp.rs の Job/JobCompletion、spawn_sandboxed_command、execute/start_command/store_job/stop_job に同一 scope を接続する。 | `cargo test --bin temote-mcp activity_job`。foreground、job返却、stop/自然完了の両勝順、session stop、timeout。後述の競合手順を使う。 |
| [ ] | S13 / S12 | spawn_local_agent / spawn_dev_tool に S12 の scope/result 確定手順を適用する。 | `cargo test --bin temote-mcp activity_delegated_job`。既存 fake executable で正常/deny/timeout/stop、prompt/output sentinel。sandbox と起動 argv は変更しない。 |
| [ ] | S14 / S11,S13 | 未計測の同期 tool と integration を coverage 表どおり追加する。1Password、kintone、Codex は外側の call だけ。 | `cargo test --bin temote-mcp activity_coverage`。一覧の各 tool に対象/除外/owner/fixture を対応付ける。Codex accepted と turn completion を区別する。 |
| [ ] | S15 / S05,S10 | 残る permission/restart/crash/auto-restart/forget/upgrade の lifecycle 計測を追加する。 | `cargo test --bin temote-mcp activity_lifecycle` と既存 upgrade tests。restart 二重発行なし、別 instance、exec後の履歴消失。upgrade成功の旧 terminal を補完しない。 |
| [ ] | S16 / S01〜S15 | 全受け入れ条件の照合、英日 docs、CHANGES.md、必要な参照を更新する。 | 親の通常 gate、public/gateway non-exposure、複数 viewer、privacy、対応 OS の手動確認。全条件の証拠が揃ったら親を完了にする。 |

表の test filter は新しく追加する test/module 名の指定であり、既存 test の存在を主張するものではない。
filtered test が0件なら検証成功として扱わない。

### 難所で採用する実装手順

#### S03: replay/live と gap

`subscribe_snapshot(filter, tail)` は `ActivitySubscription { snapshot, cutoff, receiver, permit }` を返す1つの入口にする。
broker lock 内で subscriber 登録 → cutoff 取得 → filtered tail コピーの順に行い、通信は一切しない。
subscriber 側の cursor は cutoff で初期化し、cutoff 以下を送らない。
live event は filter 判定前に global cursor を進める。

最小 fixture は、受理済み1〜3で subscribe → replay 1〜3 → publish 4と5である。
出力が1〜5で各1回になることを barrier で検証する。
別 fixture で4を別 session、5を対象 session にし、対象表示が1〜3と5でも gap を出さないことを確認する。
broadcast lag のみを発生させ、次に読めた event の直前までを global gap として通知する。
必要なら複数回の Lagged を合算するが、filter の除外件数は加算しない。

#### S11: approval policy を変更しない接続方法

既存の `ensure_local_approval()` の引数・戻り値を維持する。
scope を受け取る `ensure_local_approval_with_activity(..., Option<&ActivityScope>)` を追加し、旧関数は None を渡す wrapper にする。
既存 `local_approval(mode, class)` の Skip/Request/RequestUser の選択は変えない。
Request/RequestUser の送信直前だけ waiting を発行する。
戻り値を受けた呼び出し側が、true なら実処理直前に running、false なら failed を確定する。
まだ計測していない呼び出し箇所をまとめて書き換えず、各単位で対象の call site だけ scope を渡す。

#### S12: job と terminal の一回性

`spawn_sandboxed_command()` の前に作った scope を worker と Job が共有する。
foreground timeout で store_job() に移るときは同じ scope を渡し、別 ID を作らず、completed も出さない。
worker の tokio::select! は既存 result と別に固定の activity outcome を返す。
session stop/timeout を raw error 文字列から逆算しない。

既存 JobCompletion の mutex を結果確定の直列化点として使う。
自然完了ではその lock 内で既存 completion を保存し、scope の terminal を確定する。
stop_job では job を取り出した後、同じ completion lock の下で結果が既にあれば元操作の terminal を変えず、未確定なら cancelled の発行を試みてから abort する。
completion lock 内の activity 処理は同期かつ非待機の queue 投入に限定し、socket I/O や await を入れない。
lock の順序は completion → scope とし、逆順に取得しない。
stop_job 自身の既存 response と completion/evidence/TTL の形式は維持する。

barrier で「完了結果保存が先」「停止側の確定が先」の両方を作り、元操作の terminal が1回だけであることを確認する。
S13 はこの手順を local agent/dev tool の2つの worker へ適用するだけにする。

#### S09: 端末処理の切り離し

renderer は完成した event と表示用の時刻文字列から1行を返す pure function とし、TTY、stdin、socket、stdout に触れない。
local timezone の変換と I/O は CLI adapter で扱う。
出力待ちで Ctrl-C が効かなくなる問題は CLI adapter の pipe fixture だけで再現し、broker/approval を同時に変更しない。
no-follow と follow の両方が完成してから root command を接続する。

### 文書と完了記録

docs/managed-sessions.md と日本語版に CLI、local-only boundary、retention、loss、終了条件を記載する。
docs/usage.md と日本語版には必要な参照を追加し、README は短いまま保つ。
skills/temote-mcp/SKILL.md は local human CLI の追加だけでは変更不要とし、MCP agent の操作手順も変わる場合だけ更新する。

各単位の完了時には次の形式で注記へ記録する。

```text
Sxx: done
Changed: <files>
Verified: <command / executed test count / result>
Interface for next step: <actual types and functions>
Remaining: <remaining Sxx; unresolved facts if any>
```

## 受け入れ条件

- [ ] parser/help/dispatch が canonical syntax を扱い、tail の0、100、1024、範囲外、値欠落、未知 option を定義どおり処理する。
- [ ] 2 session の履歴があるとき、全体表示と完全一致 filter が一致し、tail は filter 後の最新 N 件を昇順で返す。存在しない有効 ID は空履歴として扱う。
- [ ] stdin を閉じた no-follow が終端 marker 後に0で終了し、非 TTY stdin の follow は継続する。Ctrl-C/TTY EOF/BrokenPipe/socket EOF/不正 frame の終了条件を確認できる。
- [ ] 通常配送時、replay/live 境界に重複・欠落がなく、複数 viewer の履歴と live sequence が整合する。
- [ ] 少ない履歴、ring eviction、意図的な tail 制限、session filter、broadcast lag を区別し、gap を対象 session の正確な欠落件数として表示しない。
- [ ] 全 operation が対象/除外一覧と整合し、dev_tool_run、読み取り、integration、lifecycle の計測漏れや二重発行がない。
- [ ] ask の操作が started/waiting/running/terminal を同一 ID で示し、deny では running を発行しない。agent/yolo の local prompt 省略と child approval の既存契約が変わらない。
- [ ] execute の foreground 完了と job 化、start_command、local_agent_run、dev_tool_run を同じ scope で実処理の terminal まで追える。job ID 返却だけで completed を出さない。
- [ ] stop_job と自然完了の競合で元操作の terminal 発行試行は1回だけであり、session stop/lifetime timeout は固定理由の cancelled として発行を試みる。既に runtime が消失した場合の配送失敗や強制 process 終了時には成功等を補完しない。
- [ ] codex_task_start/control の completed は受付結果として表示され、Codex turn の完了や現在の running 状態と誤認させない。
- [ ] malformed/oversized/unknown-schema ingress、同名 session の古い expected instance、retired sink を拒否し、session/runtime/approval の結果へ影響しない。
- [ ] event/summary/ring/queue/viewer 数の上限を満たし、viewer 不在・飽和・slow writer でも producer が配送 I/O を待たない。broker 前の欠落を仕様に明記する。
- [ ] secret sentinel を引数、path、remote 名、stdout/stderr、prompt、environment、raw error に入れても、新 event、queue、ring、broadcast、診断、CLI 出力へ混入しない。意図的に表示する session routing identity は検証入力と分離する。
- [ ] activity 接続の追加 byte が approval response/control mutation として処理されず、切断後も session socket probe と既存 job が従来どおり動作する。
- [ ] 旧 supervisor への新 viewer は deadline 内に非対応と分かり、新 supervisor の既存 CLI/control/upgrade は互換のままである。public MCP/HTTP/gateway に新規閲覧 surface がない。
- [ ] session restart は別 instance として履歴を維持し、supervisor restart/exec は generation と history をリセットする。activity の durable file が作られない。
- [ ] 英日 docs と CHANGES.md に利用方法、privacy、観測の限界、切断・再接続が記載され、通常 gate が通る。

## テスト計画

### 単体・golden tests

- activity/contract.rs・history.rs・broker.rs・scope.rs：固定 clock/UUID を注入し、serde、duration、state/terminal の一回性、UTF-8 byte 上限、control character、sequence overflow、ring の count/byte eviction を検証する。
- 固定 JSON fixture と plain-text renderer の golden assertion で、attach/activity/end/gap、空履歴、time zone、固定 error classification を確認する。新しい snapshot framework は必須にしない。
- tail 0/1/1024、他 session のみの履歴、global eviction flag、filter による sequence の飛び、replay 中の broadcast lag を小さなテスト用 capacity で再現する。
- bounded channel、barrier、oneshot 等で publish と snapshot、worker 完了と stop_job を競合させる。固定 sleep だけに順序を依存させない。時間を要する上限はテスト用 clock/deadline で短縮する。
- approvals.rs：expected instance、再作成直後の stale producer、sink 無効化、ACK timeout、未知 field、oversized frame、quiesce 中の通知を検証する。
- ask/agent/yolo の既存 policy tests に scope の assertion を追加する。approval 無し・deny・queue full・session stop の既存結果を変えないことを確認する。
- safe constructor と実際の error 経路へ sentinel を入れる。MCP/child の既存出力と、新しい viewer 出力の privacy assertion を混同しない。

### 結合・CLI tests

- [tests/cli_session_e2e.rs](../../tests/cli_session_e2e.rs) の ChildGuard、MCP client、temporary state home、socket namespace の構成を再利用し、専用 test または新しい tests/activity_e2e.rs を追加する。実利用中の supervisor/session に接続しない。
- 2 session で file read/write、command 成功/非零終了、background job、stop を実行し、replay/filter/follow と session isolation を確認する。
- Git は temporary repository と local bare remote を使い、ネットワーク認証を不要にする。agent/dev tool/1Password/kintone/Codex は既存 fixture の fake executable/transport を使う。
- EOF/SIGINT/slow stdout、2 viewer と上限超過、oversized control input、後続 allow/stop frame で当該 attachment だけが終了することを検証する。
- 旧 supervisor を模した socket fixture が未知 command を拒否/無応答にした場合を検証する。既存の protocol capability と same-PID upgrade/restore tests も実行する。
- 更新済み MCP producer と supervisor で全 coverage を検証する。session_info と runtime socket probe で viewer 切断前後の生存を確認する。
- src/mcp.rs の public tools tests、src/http.rs の route tests、src/gateway.rs と gateway の contract tests で activity が公開されていないことを検証する。

### 実行コマンドと手動確認

実装後に以下を実行する。

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
(cd gateway && npm test)
git diff --check
```

本変更は MCP の共通 dispatch と内部 protocol に触れるため、gateway tests も実施する。
Linux と対応 macOS の端末で local timezone、一行表示、Ctrl-C、TTY EOF、pipe、supervisor upgrade 後の再接続を確認する。
一方の OS しか利用できない場合は未実施の OS を記録し、CI または別 host で確認する。
外部サービスの credential を必要とする live 操作は必須 gate とせず、既存 integration fixture で本機能の受け入れ条件を検証する。
今回のイシュー整備では実装が存在しないため、上記 runtime tests は未実施である。本文の形式、参照先、差分を検証する。

## リスク

- 既存 free-form activity を転用すると秘密を保持する。新経路は型付き入力と固定分類に限定し、保存前に検証する。
- 送信 queue と broker の間で欠落し得る。sequence gap だけで完全性を保証せず、header/docs に best-effort と明示する。
- runtime、MCP process、job worker の所有関係を混同すると二重 terminal や cancellation の漏れを生む。scope の所有・移譲を明示し、既存結果確定箇所で計測する。
- session ID の再利用で履歴が混ざる。expected instance の ingress 照合と表示用 instance UUID の両方で防ぐ。
- slow viewer が lock や socket writer を占有する。snapshot サイズ、subscriber 数、write deadline を独立して制限する。
- control version の不用意な変更で既存 upgrade が非互換になる。追加 request/schema として扱い、protocol 2 の既存契約を回帰検証する。
- 広い tool coverage の実装途中では抜けが起きやすい。明示一覧、発行責任、fixture で全対象を照合する。

## 変更履歴

`CHANGES.md` impact: yes（機能実装時）。イシュー本文の整備だけでは CHANGES.md を変更しない。

項目案：

- supervisor の最近の操作を、同一ユーザーの temote-mcp activity から履歴表示・追跡できるようにする。履歴はメモリ内の上限付き best-effort で、supervisor restart 時に消去される。

package version は CalVer workflow が管理するため、この実装のために手動更新しない。

## 注記

- 2026-09-14: S04 の実装・検証を開始した。S01〜S03 の公開 API、wire contract、strict decode、固定エラー、history、snapshot/gap を変更せず、`activity/scope.rs` の pure な同期 scope/emitter 層だけを追加する。producer queue、broker adapter、socket、runtime、approval、job、CLI、public/gateway は対象外とする。
- 2026-09-14: S04: done。親の Status は `polished` のまま、S01〜S03 の完了・レビュー記録を保持し、S05〜S16 は未実装のままとする。Changed は `src/activity.rs`（`scope` module 登録）、`src/activity/scope.rs`（新規 pure scope/emitter）、本イシュー記録のみ。実 API は `ActivityEmitter::try_emit(ActivityUpdate) -> Result<(), ActivityEmitError>`、`ActivityMonotonicClock`、`ActivityScope::new` / `with_summary` / `with_clock` / `with_summary_and_clock`、`operation_id` / `operation` / `started_at` / `started_emission` / `state` / `is_terminal`、`transition`、terminal 限定 `finish`、`running` / `waiting_approval` / `complete` / `fail` / `cancel` と typed summary variant である。`ActivityScope` は通常経路で UUID と開始 `Instant` を生成し、開始時に `started` を一度だけ試行する。Clone は同じ scope state を共有し、emitter 拒否は `ActivityEmission::Rejected` として返すが状態・terminal 確定を戻さない。状態と emitter 呼び出しは mutex 内で同期し、非再入・非待機の emitter 契約、terminal 後の抑止、Drop からの terminal 推測なし、S01 の typed summary 限定、単調時間の u64 milliseconds 飽和を実装した。
  追加・修正テストは11件（`started_is_attempted_once_and_shared_handles_keep_identity`、`normal_and_short_terminal_paths_have_typed_states_and_durations`、`approval_waiting_running_and_denial_paths_are_explicit`、`repeated_nonterminal_state_is_a_noop_and_finish_rejects_nonterminal`、`terminal_state_suppresses_all_later_updates`、`emitter_rejection_is_reported_without_changing_scope_policy`、`completed_wins_a_controlled_terminal_race`、`cancelled_wins_a_controlled_terminal_race`、`concurrent_nonterminal_and_finish_never_emits_nonterminal_after_terminal`、`duration_uses_monotonic_time_and_saturates_without_instant_overflow`、`drop_does_not_infer_a_terminal_event`）。`cargo test --lib activity::scope` は 11 passed / 0 failed / 88 filtered、`cargo test --lib activity::broker` は 19 passed / 0 failed / 80 filtered、`cargo test --lib activity::history` は 8 passed / 0 failed / 91 filtered、`cargo test --lib activity::contract` は 17 passed / 0 failed / 82 filtered で、filter 0件はない。scope テストは単一スレッドで20回反復し各回11 passed（計220実行）、Astra の再レビューでも100回反復・1,100 passed / 0 failedと報告された。Astra は当初、scheduler 任せの terminal race と無効な `WaitingApproval → Completed` を使う断続的 panic を changes-requested と指摘したが、同期 gate、両方向の先勝ち検証、有効な `cancelled` 競合、結果 assertion に修正し、`gpt-6-astra` の再レビューは approve となった。要求された `gpt-6.0-astra` は実行環境に存在せず、実際の reviewer 識別名は `gpt-6-astra` である。`cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`、Issue root `/home/hirohito-fujita/src/local-mcp/issues` の issues CLI validate（violations なし）は成功した。no-default-features の既存 dead-code warning は `approvals.rs`、`profile.rs`、`session_control.rs` の binary 側で S04 由来ではない。修正後の全 `cargo test` は library 99 passed、sandbox 1 passed、binary 727 passed / 1 failedで、既存の `http::tests::public_managed_session_lifecycle_uses_named_root_and_existing_tools` が `src/http.rs:1173` の session_list assertion で失敗した。`TEMOTE_MCP_SOCKET_NAMESPACE=s04-recheck cargo test --bin temote-mcp http::tests::public_managed_session_lifecycle_uses_named_root_and_existing_tools -- --test-threads=1` でも 0 passed / 1 failed で再現した。S04 の差分は HTTP/session backend/public route に触れないため無関係な既存失敗として修正していない。gateway `npm test` は gateway/shared protocol 未変更のため未実施。Interface for next step は `ActivityScope` の共有 handle、typed `ActivityEmitter`、`ActivityEmission`（accepted/rejected/not attempted）、`ScopeError`、`ActivityMonotonicClock` と、broker 接続時に `transition` / `finish` へ S01 の `ActivitySummary` を渡すこと。CHANGES.md impact は no（S04 は内部 pure scope/emitter API、利用者向け文書更新は S16）。Remaining は S05〜S16、cancelled の job/session 固有理由分類、S04 から runtime/approval/job/MCP/CLI/socket/broker adapter への接続、上記の対象外 HTTP test failure である。
- 2026-09-14: 現行コードと関連イシューを照合し、runtime が supervisor 内のタスクであること、job が MCP 側にあること、現在の control protocol が2であることを確認した。
- 2026-09-14: v1 の coverage、scope と terminal の所有、instance binding、bounded delivery、global gap、tail/EOF/互換性を具体化した。実装を妨げる未解決の質問はない。
- 2026-09-14: 正確な実行モデル識別名を確認できないため Model: unknown とする。Branch は今後の実装用候補であり、この整備では実装・branch 作成・commit・push を行わない。
- 2026-09-14: 現行コードに基づき実装範囲、配送と欠落、privacy、互換性、受け入れ条件、テスト計画を確定したため、実装可能な polished へ移動する。
- 2026-09-14: 実装担当 gpt-5.6-luna / reasoning effort max を想定し、全体を S01〜S16 の依存付き作業単位へ分割した。最初の pure contract は独立子イシューとし、並行処理・承認・job 取消は別単位に固定した。これは実装手順の整理であり、当該モデルによる実装検証はまだ行っていない。
- 2026-09-14: S01: done。子イシューを `issues/done/20260914-activity-event-contract.md` へ移動し、`temote_mcp::activity::contract` の実 API、11件の指定テスト、通常全体テスト、clippy、format、no-default-features build、diff check の結果を記録した。S02〜S16 は未実装であり、親の完了条件は未達のままとする。
- 2026-09-14: S01 レビュー指摘の再検証を開始した。子イシューの状態は既存規約どおり `done` を維持し、指摘1〜3の修正・再検証結果を子イシューへ追記する。
- 2026-09-14:
  S01 レビュー指摘: resolved。子イシューは `done` を維持し、親の Status は `polished`、S02〜S16 は未実装のままとする。実装 API は `temote_mcp::activity::contract` の `ActivityOperation`、`ActivityState`、`ActivityErrorKind`、`ActivityRemote`、`ActivitySummary`、`ActivityUpdate::new` / `with_schema_version`、`EventStamp::new`、`ActivityEvent::from_update`、`encode_update` / `decode_update`、`encode_event`。
  修正・検証内容は、operation/state の object・array・number・bool・null 拒否、update トップレベルと summary 内の重複 JSON key 拒否、`duration_ms` / event の `session_id`・`session_instance`・`duration_ms` の explicit null と key presence 検査。raw duplicate bytes による修正前再現は 0 passed / 2 failed、修正後の `review_reproduces_*` は 2 passed。`cargo test --lib activity::contract` は 17 passed、全 `cargo test --quiet` は library 61、binary 728、SDK 3、integration 13 passed、3 ignored、0 failed。
  `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`、issues CLI validate（violations なし）は全て成功。gateway `npm test` は S01 の変更範囲外のため親の統合段階で実施する。S02〜S16 の history、broker、scope、transport、CLI、runtime 接続、利用者向け docs/CHANGES は未実装・未変更。
- 2026-09-14: S02 の実装・検証を開始した。既存の `done`/`polished` 配置を維持し、count/byte bounded history、filtered tail、global `history_truncated` だけを `activity/history.rs` に追加する。lock、broadcast、subscriber、sequence/generation 自動生成、transport、CLI、S03 以降は対象外とする。
- 2026-09-14: S02: done。親の Status は `polished`、S01 の `done` 記録は維持し、S03〜S16 は未実装のままとする。Changed は `src/activity.rs`（`history` module 登録）、`src/activity/history.rs`（新規 pure bounded history）、本イシューの S02 記録であり、S01 の `src/activity/contract.rs`、公開 wire contract、strict decode、固定エラー、回帰テストは変更していない。
  実 API は `temote_mcp::activity::history::{ActivityHistory, HistoryError, MAX_ACTIVITY_HISTORY_EVENTS, MAX_ACTIVITY_HISTORY_BYTES, MAX_ACTIVITY_TAIL}`。`ActivityHistory::new()` は本番上限（4096 events、8 MiB）を使い、`with_limits(max_events, max_bytes)` はテスト用に 1..=4096 と 1..=8 MiB の範囲だけを受理する。`push(event, serialized_size)` は検証済み `ActivityEvent` と `encode_event(&event)` の末尾 LF なし bytes 長を受け取り、event 検証、S01 の 2048 bytes 上限、サイズ一致、履歴 byte 上限を変更前に確認する。不正入力・単体で履歴上限を超える入力は `HistoryError` の固定 variant で拒否し、既存 event、byte 合計、`history_truncated` を変更しない。count/byte の両方が収まるまで最古から必要数だけ eviction し、checked arithmetic で byte accounting を守る。`tail(session_id, limit)` は `limit` 0..=1024、session filter は完全一致かつ session-independent event を除外し、最新 N 件を sequence 昇順で返す。session lookup、metadata、起動、probe、raw JSON、任意 summary、raw error、lock、broadcast は保持・実行しない。`history_truncated` はこの history lifetime で eviction が一度でも起きたときだけ true になり、読み取り・filter・tail 制限・履歴不足では変化しない。
  追加テストは `count_capacity_evicts_only_the_oldest_events`、`byte_capacity_evicts_until_the_new_event_fits`、`one_append_can_evict_multiple_oldest_events`、`exact_byte_limit_is_accepted_and_single_event_overflow_is_rejected_without_mutation`、`invalid_capacity_and_serialized_size_are_rejected_without_mutation`、`filtered_tail_uses_exact_session_matches_and_sequence_order`、`truncation_is_global_and_read_operations_do_not_mutate_history`（空履歴も検査）。`cargo test --lib activity::history` は 7 passed / 0 failed / 61 filtered、`cargo test --lib activity::contract` は 17 passed / 0 failed / 51 filtered で、filter はいずれも 0件ではない。`cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`、Issue root `/home/hirohito-fujita/src/local-mcp/issues` の issues CLI validate（violations なし）も成功した。no-default-features check の既存 dead-code warning は `approvals.rs`、`profile.rs`、`session_control.rs` の binary 側で、S02 由来ではない。gateway の `npm test` は gateway/shared protocol を変更していないため未実施とした。
  `cargo test` は S02 の lib tests を含む 68 tests が通過した後、既存の `http::tests::public_managed_session_lifecycle_uses_named_root_and_existing_tools` で 727 passed / 1 failed となった。`TEMOTE_MCP_SOCKET_NAMESPACE=s02seq cargo test -- --test-threads=1` でも同じ失敗を再現し、`SessionBackend::InProcess` の session_start と global `session_views_for_mcp()` の session_list が別経路であることを確認した。S02 の変更ファイルはこの経路に触れていないため、無関係な修正は行っていない。`cargo test --lib` の S01/S02 対象テストおよび S02 の API 受け入れ条件は成功している。
  Interface for next step: S03 は `ActivityHistory::push` の完成 event/既知 envelope size 入力、`tail` の optional exact session filter と 0..=1024 limit、`history_truncated`、`len`/`serialized_bytes` を broker の ring/replay 実装から利用できる。push の入力条件は event が S01 `ActivityEvent::validate()` を通り、`serialized_size == encode_event(event).len()`、event size が 2048 bytes 以下、history byte limit に収まること。CHANGES.md impact は no（S02 は内部 pure in-memory retention、利用者向け文書は S16）。Remaining は S03〜S16 と、上記の既存 HTTP test の無関係な環境/経路問題である。
- 2026-09-14: S02 レビューで、`tail(None, 1)` が全一致 event を clone してから件数を絞るため、4096件保持時に返却上限を超えてコピーする指摘を受領した。S02 の完了記録は維持したまま、指摘箇所の最小修正と回帰検証を行った。
- 2026-09-14: S02 レビュー指摘: resolved。`tail` は filter と sequence sort を `HistoryEntry` 参照だけで行い、最新 `limit` 件を選別した後にだけ event を clone する private `tail_with` 経路へ変更した。1025件の履歴に対する `tail(None, 1)` の返却 sequence `[1025]` と clone 回数 1、`tail(None, 1024)` の返却 sequence `[2..=1025]` と clone 回数 1024 を `tail_selects_before_cloning_when_history_exceeds_the_replay_limit` で確認し、snapshot の event コピー数が `limit` 以下であることを固定した。`cargo test --lib activity::history` は 8 passed / 0 failed / 61 filtered、`cargo test --lib activity::contract` は 17 passed / 0 failed / 52 filtered。`cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`、Issue root `/home/hirohito-fujita/src/local-mcp/issues` の issues CLI validate（violations なし）も再検証成功。no-default-features の既存 dead-code warning は `approvals.rs`、`profile.rs`、`session_control.rs` の binary 側で、S02 由来ではない。S01 の API/wire contract/strict decode/固定エラー/回帰テスト、親の `Status: polished`、S01 `done`、S03〜S16 未実装、CHANGES.md impact no は維持する。
- 2026-09-14: S03 の実装・検証を開始した。S01/S02 の公開 API と完了記録を保護し、`activity/broker.rs` に bounded history/live broadcast、受理順 sequence、注入 clock/generation、atomic snapshot+subscribe、global broadcast gap、subscriber permit 上限だけを追加する。S04 以降、runtime ingress、scope、socket、CLI、approval、job、public/gateway、利用者向け文書は今回の対象外とする。
- 2026-09-14: S03: done。親の Status は `polished` のまま、S01/S02 の完了・レビュー記録を保持し、S04〜S16 は未実装とする。Changed は `src/activity.rs`（`broker` module 登録）、`src/activity/broker.rs`（新規 bounded broker）、本イシュー記録のみ。`ActivityBroker::new(clock, generation)` は本番上限（history 4096/8 MiB、broadcast 1024、subscriber 16）で開始する。`ActivityBroker::with_limits(empty_history, broadcast_capacity, max_subscribers, clock, generation)` はテスト用の小さい上限を受け付けるが、sequence origin を外部から設定できないよう empty history だけを受理する。`ActivityClock` は同期 clock source、generation は constructor 注入 UUID とし、受理順 sequence は1開始・`u64::MAX` 後に停止する。`publish(update, session_id, session_instance)` は S01 の typed `ActivityUpdate` を検証し、broker timestamp、`ActivityEvent::from_update`、`encode_event` の known size、S02 `ActivityHistory::push`、非待機 broadcast send を同じ try-lock critical section で処理する。入力不正、lock 競合、event/history 上限、sequence exhaustion は固定 `BrokerError` で返し、receiver 不在は受理失敗にしない。`subscribe_snapshot(filter, tail)` は permit 取得、receiver 登録、cutoff、S02 filtered tail、generation、snapshot 時点の `history_truncated` を一つの lock 内で返す。`ActivitySubscription` は bounded snapshot、cutoff、global cursor、receiver、owned permit を持ち、`recv()` は filter 前に cursor を進め、実際の broadcast `Lagged` だけを `(after_sequence, through_sequence]` の `ActivityGap` として返す。tail/filter/history shortage/eviction は live gap にせず、gap endpoint の別 session event も保持する。追加した broker unit test は19件（`fixed_constructor_starts_at_one_and_keeps_typed_event_data`、`publish_without_subscribers_remains_in_history`、`clock_can_move_back_without_changing_sequence_order`、`replay_then_live_has_each_sequence_once`、`snapshot_and_publish_barrier_have_no_duplicate_or_missing_event`、`filtered_live_events_advance_global_cursor_without_a_false_gap`、`small_broadcast_capacity_reports_global_gap_and_following_event`、`repeated_lag_is_reported_without_duplicate_counting`、`history_truncation_is_snapshot_state_not_a_live_gap`、`subscriber_limit_is_bounded_and_drop_releases_permit`、`invalid_subscribe_and_lock_contention_do_not_leak_or_mutate_state`、`invalid_publish_is_rejected_before_sequence_or_history_change`、`invalid_capacities_are_rejected`、`nonempty_history_cannot_set_the_broker_sequence_origin`、`sequence_overflow_stops_without_wrap_or_partial_update`、`session_filter_and_tail_boundaries_are_read_only`、`replay_lag_reports_global_gap_before_an_excluded_event`、`consecutive_lagged_notifications_are_accumulated_once`、`multiple_subscribers_receive_each_sequence_once`）。`cargo test --lib activity::broker` は 19 passed / 0 failed / 69 filtered、`cargo test --lib activity::history` は 8 passed / 0 failed / 80 filtered、`cargo test --lib activity::contract` は 17 passed / 0 failed / 71 filtered で、各 filter は0件ではない。`cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`、Issue root `/home/hirohito-fujita/src/local-mcp/issues` の issues CLI validate（violations なし）は成功した。no-default-features の dead-code warning は既存の `approvals.rs`、`profile.rs`、`session_control.rs` の binary 側で S03 由来ではない。全 `cargo test` は activity を含む library 88 passed、sandbox 1 passed、既存の `http::tests::public_managed_session_lifecycle_uses_named_root_and_existing_tools` で 727 passed / 1 failed。単体再現 `TEMOTE_MCP_SOCKET_NAMESPACE=s03-recheck cargo test --bin temote-mcp http::tests::public_managed_session_lifecycle_uses_named_root_and_existing_tools -- --test-threads=1` も 0 passed / 1 failed で同じ `src/http.rs:1173` assertion になった。S03 の差分に `src/http.rs`、HTTP/session backend、public route はなく、無関係な既存失敗として修正していない。gateway `npm test` は gateway/shared protocol 未変更のため未実施。Interface for next step は S04 が `ActivityBroker::publish` の typed update/session identity 入力、`subscribe_snapshot` の filtered snapshot/cutoff/generation/truncation、`ActivitySubscription::recv` の `ActivityDelivery::{Event,Gap}`、`ActivityGap` の区間 getter、固定 `BrokerError::Busy` 等、owned permit の Drop 解放を利用できること。CHANGES.md impact は no（S03 は内部 in-memory API、利用者向け更新は S16）。Remaining は S04〜S16 と上記の対象外 HTTP test failure である。
- 2026-09-14: S04 の独立レビューを `gpt-6-astra` / `xhigh` で再確認した（指定の `gpt-6.0-astra` は実行環境に存在しない）。判定は実装上の blocker なしの approve-with-nits で、正常コンストラクタの fresh UUID、terminal 先勝ち時の後続非 terminal 抑止、emitter 拒否後の attempts 不変を確認する回帰テストを追加した。`cargo test --lib activity::scope -- --test-threads=1` は 13 passed / 0 failed / 88 filtered。追加テスト後の `cargo test --quiet -- --test-threads=1` は library 101 passed、sandbox 1 passed、binary 727 passed / 1 failedで、既存の `http::tests::public_managed_session_lifecycle_uses_named_root_and_existing_tools` が `src/http.rs:1173` の session_list assertion で再現した。既存 S01〜S03 の contract/history/broker テストと公開 API、親の `Status: polished`、S05〜S16 未実装を維持する。Vite+ の残る実環境確認は `issues/open/20260908-live-acceptance-matrix.md` に明示した。S04 の追加テスト差分以外に S04 実装の変更はなく、CHANGES.md impact は no のままとする。
- 2026-09-14: S04 の再レビューを `gpt-6-astra` / `xhigh` で実施し、approve を得た。指定の `gpt-6.0-astra` は実行環境に存在しないため、実行可能な識別名を記録している。レビュー中に、別 issue の未達 CLI 条件を `done` としていた誤りが判明したため、`20260911-gateway-deployment-target.md` を `issues/open` へ戻し、`target_missing` の command-level 修正と回帰テストが必要であることを記録した。S04 自体の追加指摘はなく、S04 scope テスト13件、activity合計57件、対象 issue 6件の形式検証、65件のリンク/参照確認は成功した。親の `Status: polished`、S05〜S16 未実装、CHANGES.md impact no、既知の HTTP test failure は維持する。
- 2026-09-15: S05: done。Changed は `src/activity/contract.rs`（strict な既存 wire visitor を再利用する `ActivityUpdate` serde 実装。既存 `encode_update` / `decode_update` の validation と固定 `ContractError` 分類は維持）、`src/approvals.rs`（strict `activity_update` ingress、expected session identity、instance-bound sink、固定 ACK、bounded ACK task、retirement）、`src/supervisor.rs`（process lifetime の broker と runtime ごとの UUID）、本イシュー記録。runtime は producer supplied の ID / `started_at` / `process_id` を sink の captured identity と照合し、一致時だけ canonical session ID と supervisor generated instance UUID を付けて broker へ publish する。sink publish は `try_lock`、broker publish も非待機で、busy / stale / retired / invalid update は操作結果へ影響せず破棄する。retire は同じ mutex で直列化し、stop/reap/shutdown の map removal 前と runtime task 終了時に実行する。ACK writer は既存 session read permit を task 完了まで保持するため同時最大64、frame ごと5秒 deadline で、切断は runtime へ波及しない。upgrade quiesce 中も typed update は観測通知として処理する。旧 free-form `Activity` と他の session message の permissive field handling、approval registration、MCP dispatcher、public HTTP/gateway surface は変更していない。
  追加した `activity_ingress_*` は7件で matching identity/instance stamp、stale、retired、8 MiB 超 frame、unknown/duplicate field、unknown schema、ACK peer disconnect、旧 `Activity` variant の追加 field 互換性を直接 Unix socket fixture で確認した。`cargo test --bin temote-mcp activity_ingress` は7 passed / 0 failed / 728 filtered、`cargo test --lib activity::contract` は17 passed / 0 failed / 84 filtered。全 `cargo test -- --test-threads=1` は library 101、binary 735、SDK 3、integration 1+1+2+9 passed、process-boundary 3 ignored、0 failed。`cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check` は成功した。no-default の5件の dead-code warning は従来の `approvals.rs`、`profile.rs`、`session_control.rs` に限られる。`gpt-6-astra` review は source diff `a82f3a4a88912f1011afd74623daab9aa3176e1d1bb02ca4998aeef3a939f0e6` を approve。clippy 指摘を解消する private argument grouping 後の source diff は `eaf1123bcdf1d652ca164e358d44fcccd67a8e80e01f52ccaed70bbc3ba23836` で、対象 test / format / clippy / no-default / diff gate を再実行した。Interface for next step は `approvals::RuntimeActivity::new(broker, session_instance)`、supervisor の process-wide `activity_broker()`、strict `Message::ActivityUpdate` ingress と固定 `accepted` / `discarded` ACK、既存 `ActivityUpdate` の strict serde。CHANGES.md impact は no（S05 は未公開の内部 ingress で、利用者向け記録は S16）。Remaining は S06〜S16、process-wide producer queue、viewer transport/CLI、operation coverage、lifecycle、docs である。
- 2026-09-15: S06: done。Changed は `src/activity_runtime.rs`（process-wide bounded producer、単一 ordered worker、runtime 用 `ActivityEmitter` adapter と固定 delivery error）、`src/approvals.rs`（S05 の strict `Message::ActivityUpdate` encoder と expected identity helper の crate 内共有）、`src/main.rs`（module 登録）、本イシュー記録。producer は容量256の `mpsc` に `try_send` し、操作 thread を待たせない。enqueue 時に config から socket path と session ID / `started_at` / `process_id` を一度だけ capture し、worker は後から session state を読み直さない。単一 worker は connect、checked encode、write/shutdown、`accepted` / `discarded` ACK read の全体を1秒 deadline 内で順番に実行する。失敗時の retry はなく、raw OS error/path は event、stderr、操作結果へ出さず固定 error variant に閉じる。delivery の成否は元の操作結果へ影響しない。実操作への scope 接続は S10 以降に残した。
  `activity_producer_*` は5件で受信順序と captured identity、容量256の full/closed、1秒 timeout と no retry、raw path sentinel 非表示、bad ACK 後の次 item 継続を直接 Unix socket fixture で確認した。`cargo test --bin temote-mcp activity_producer` は5 passed / 0 failed / 735 filtered、回帰として activity ingress 7件と contract 17件も成功した。全 `cargo test -- --test-threads=1` は library 101、binary 740、SDK 3、integration 1+1+2+9 passed、process-boundary 3 ignored、0 failed。`cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check` は成功した。no-default の5件の dead-code warning は従来の `approvals.rs`、`profile.rs`、`session_control.rs` に限られる。`gpt-6-astra` review は base `523435b`、source diff `8b1ec6b0b29aa8cdaffb95d6358abf37514bfd09fa4a3521a0154d5efc12c250` を approve し、指摘なし。Interface for next step は `activity_runtime::emitter(&Session)` が返す非待機 `ActivityEmitter` と、S05 broker の attachment transport。CHANGES.md impact は no（S06 は未公開 producer で、利用者向け記録は S16）。Remaining は S07〜S16、viewer attachment/client/CLI、実操作 coverage、lifecycle、docs である。
- 2026-09-15: S07: done。Changed は `src/session_control.rs`（strict `AttachActivityRequest`、control dispatcher の独立 attachment branch、broker replay/live transport、固定 attach error と境界 fixture）、本イシュー記録。request は `command`、`schema_version`、nullable `session_id`、`tail`、`follow` だけを受け付け、未知 field、重複 field、型違い、未知 schema を拒否する。他の既存 `ControlRequest` variant の permissive field handling は維持した。attach は `dispatch_request()` とその `reap_finished()`、approval registration を通らず、supervisor の process lifetime broker から subscriber permit、generation、cutoff、filtered tail、global truncation を atomic に取得する。成功応答は control protocol 2 / activity schema 1 の既存 `ControlResponse` 形式で、replay を sequence 昇順に送り、follow の有無にかかわらず `activity_end` を一度送り、follow では以後の event と actual broadcast lag の `(after_sequence, through_sequence]` global gap を送る。
  初回 read と各 frame write は5秒で制限する。request 後の EOF または1 byte 以上の入力は viewer だけを終了し、同じ connection の allow/stop/別 control request を dispatch しない。初回 line reader が先読みした pipelined input も検出して attachment を開始せず閉じる。socket directory/mode、subscriber 最大16、tail 0..=1024、ring/broadcast の既存上限を再利用し、public HTTP/gateway、control protocol version、approval console、CLI parser は変更していない。S08/S09 の client は replay/follow 中に socket write half を保持し、初回 request 後に shutdown しない必要がある。
  `activity_attachment_*` は9件で filtered replay/end、real dispatcher 経由の follow と後続 stop、global gap、strict unknown/duplicate field と legacy variant 互換、unknown schema、旧 peer close、pipelined allow、5秒 initial read timeout、malformed typed value/unknown key sentinel の固定 parse error を確認した。`cargo test --bin temote-mcp activity_attachment -- --test-threads=1` は9 passed / 0 failed / 740 filtered。`cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check` は成功した。no-default の5件の warning は従来箇所に限られる。privacy 修正前の全 suite は library 101、binary 748、SDK 3、integration 1+1+2+9 passed、process-boundary 3 ignored、0 failed。最終 source の全 suite は library 101 passed 後、binary 747 passed / 2 failed で、S07 が触れない既存 `generated_shutdown_denies_all_pending_approvals` と `idle_ipc_connection_does_not_block_runtime_control_or_probe` が load 下の内部 timeout に達した。両 test の直後の単独再実行は各1 passed / 0 failedで、S07 source の変更や追加失敗はなかった。`gpt-6-astra` は最初の source diff `490c51c9d4ad01d9200796d5312832882ceddb4d072305bd1ce07050b177db0f` で raw serde error が outer log に入力を残す privacy blocker を指摘した。parse failure を cause なしの固定 `invalid control request` に変更し、outer handler が形成する `{error:#}` に key/value sentinel がない回帰を追加した source diff `ae73f444c9c2bb2c09aee2552a9c6c49fd2d7c98982ce30e9f3a6b7ae150f81b` を再レビューし approve、残る指摘なし。Interface for next step は strict attach response/activity/end/gap frame と、write half を保持する finite replay client。CHANGES.md impact は no（S07 は未公開 transport で、利用者向け記録は S16）。Remaining は S08〜S16、strict event decoder/renderer/client、CLI、実操作 coverage、lifecycle、docs である。
