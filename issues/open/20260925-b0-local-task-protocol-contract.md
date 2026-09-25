# B0: local task protocol / 認証 scope / 実行所有者の契約

Status: contract delivered (設計 packet。実装コードなし)
Packet: B0 (Phase B — local frontend の第 1 child)
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `6df8181` (B0 は A4 PR #55 に依存しない)
Parent issue: `issues/open/20260924-temote-development-harness-restructure.md` (PR #47)
Prerequisites: A1 `51424a4` / A2 `9377bc8` / A3 `3e342d6` (すべて `main` merge 済み)、
`src/session_control.rs` の `CONTROL_PROTOCOL_VERSION = 2`、`config::supervisor_socket_path()`

## 1. Goal

local client (CLI / Desktop / native GUI) が MCP round-trip なしで task lifecycle を操作するための
control protocol 契約を、B1 (socket routing) / B2 (CLI + operation ID) が再設計なしで実装できる形で
固定する。認証 scope・session ownership・permission context・実行所有者・切断/lifetime・retry の
意味を decision table と request / response 例で確定する。この packet は契約書だけを成果とし、
コード変更・runtime 検証は行わない。

`--local` は transport が local であることだけを意味する。`--local != yolo`、
`--local != unrestricted filesystem`、`--local != approval bypass`、`--local != sessionless`。

## 2. Fixed decisions

### 2.1 Protocol version と互換性

- 既存の `control_protocol: u64` (現在 2) を protocol version の単一の番号として使う。
  task request は **`protocol_version` フィールドを必須**とし、値は request 時点の
  `CONTROL_PROTOCOL_VERSION` (B1 実装で **3** に上げる) とする。
- 照合は **要求機能ごとの最小 version** で行う。厳密一致は要求しない。
  - session lifecycle / permission / upgrade / activity の既存 request は v2 のまま変わらない。
    v3 supervisor は v2 client の request を引き続き受理する (additive 互換)。
  - task request は `protocol_version >= 3` を要求する。v2 supervisor に task request を送ると
    **副作用ゼロで**拒否する。client は Ping の `control_protocol` を見て、3 未満なら
    task request を送らない。
  - 既存の `local_control()` の厳密一致チェックは「その client が必要とする最小 version 以上」へ
    変更する。`serve` / `up` は最小 2、task CLI は最小 3、supervisor upgrade の capability 交換は
    引き続き厳密一致 (binary 状態を handoff するため)。
- 未知の command は既存どおり `invalid control request` として拒否する。未知 field は
  B1 で task request に限り拒否する (`deny_unknown_fields` 相当)。task request は
  `protocol_version`・`session_id`・`backend`・`operation_id` / `task_id` の型検証を
  approval と backend dispatch の前に完了する。

```json
// request (v2 supervisor へ送った場合)
{"command":"task_list","protocol_version":3,"session_id":"0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb"}
// response
{"ok":false,"result":null,"error":"control protocol 2 does not support Temote task operations (required: 3); restart the lifecycle supervisor with the matching Temote version"}
```

### 2.2 Session 識別と ownership

- task 操作は **すべて `session_id` を明示**する。task ID や cwd から session を探索して
  自動選択しない。`--session` もしくは検証済みの接続 context を必須とし、CLI が session を
  省略した場合は usage error で停止する (cwd からの推測をしない)。
- request が運ぶのは `session_id` だけとする。`started_at` / `process_id` / scope /
  permission mode を caller に指定させない。
- server は live な session instance (`session_id` + `started_at` + `process_id` + canonical
  scope) を解決し、その `config::Session` を core へ渡す。同じ `session_id` でも restart 後の
  別 instance・別 scope は別 owner であり、先行 instance の task を取得・制御できない。
- 他 session の task は存在確認も含めて返さない。ownership 不一致は backend の既存 error
  (`CODEX_TASK_NOT_FOUND: task was not found` 等) をそのまま返し、task の存在を漏らさない。

```json
// request
{"command":"task_get","protocol_version":3,"session_id":"0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb","backend":"codex","task_id":"0199cccc-cccc-7ccc-8ccc-cccccccccccc","after_revision":7}
// response (同じ権限の caller なら MCP の codex_task_get と同一 JSON)
{"ok":true,"result":{"task_id":"0199cccc-cccc-7ccc-8ccc-cccccccccccc","status":"running","revision":8,"generation":1,"evidence":null,"retention_seconds":86400},"error":null}
```

task view の field 集合はその時点の backend 実装に従う。A4 (task state separation、PR #55) の
`execution` / `verification` / `delivery` field は protocol 層で解釈せずそのまま通過する。

### 2.3 認証と permission context

- local transport の認証境界は owner-only filesystem とする: supervisor socket dir `0700`、
  `supervisor.sock` `0600`。B1 は追加で peer credential (Linux `SO_PEERCRED` / macOS
  `getpeereid`) の euid が supervisor の euid と一致することを dispatch 前に検証する。
  不一致は `TASK_PEER_REJECTED` として副作用ゼロで拒否する。
- request は permission mode / grants / approval 判断を運ばない。server は解決した session の
  **現在の** `permission_mode` と `grants` を core へ渡す。task 操作は
  `approvals::local_approval` の既存方針に従う:
  - `agent` / `yolo`: 構造化された task 操作は Temote-local prompt を skip する。
  - `ask`: `ApprovalClass::{CodexAppServer,OpenCodeServer,DevinAcp,DevinCloud}` の既存 approval を
    session の console へ送る。console 不在・busy は fail closed
    (`local approval console is unavailable or busy`) で backend side effect 0。
- caller は `session permission` 以外の方法で permission mode / scope / grants を変更できない。
  task request にそれらを追加してはならない。
- `--local` 経路は `--yolo` のみの挙動を公開しない。task 操作は MCP と同一の
  typed validation・scope・approval class を通り、local caller であることだけを理由に
  sandbox / path / network 条件を緩めない。

### 2.4 実行所有者と routing

- supervisor は session の routing / lifecycle を所有し、client 切断で session を止めない。
- 実処理は **解決済み session の実行境界**で行う。owner-only socket は transport の認証であり、
  supervisor 文脈 (ambient cwd、既定 permission、caller の cwd) で backend を起動する権限では
  ない。supervisor が backend を直接 spawn する実装にしてはならない。
- B1 の経路: supervisor が request を受信 → session instance を解決 → その session の
  executor (session runtime / session-scoped task executor) へ route → A 系の
  `orchestration::invoke(backend, operation, args, session, activity)` へ渡す。
  activity / approval / evidence / task runtime lease は MCP 経路と同じ session 文脈を使う。
- session が active でない、または instance を解決できない場合は backend を呼ばず
  `TASK_SESSION_NOT_FOUND` / `TASK_SESSION_NOT_ACTIVE` で拒否する。supervisor が代理で
  task を作らない。

### 2.5 Request / response 契約

既存 envelope を変更しない: request は 1 行 JSON、response は 1 行
`{"ok":bool,"result":json|null,"error":string|null}`。`result` は対応する MCP tool 応答と
同一 JSON (backend view / A3 の merged task list) で、既存の bound (task_list `limit` 1..=128
default 50、task view の field 集合) を守る。

B1 が追加する command (すべて snake_case、`protocol_version` 必須):

| command | 必須 | 任意 | `result` |
| --- | --- | --- | --- |
| `task_start` | `protocol_version`, `session_id`, `backend`, `operation_id`, `request.task` | backend 固有 field (`model` / `effort` / `agent` / `variant` / `cloud` / `title` / `devin_mode` / `repos` / `max_acu_limit`) | backend task view |
| `task_get` | `protocol_version`, `session_id`, `backend`, `task_id` | `after_revision` | backend task view (`not_modified` 含む) |
| `task_list` | `protocol_version`, `session_id` | `limit` | A3 merged `{tasks, backends, total, truncated, limit}` |
| `task_control` | `protocol_version`, `session_id`, `backend`, `task_id`, `operation_id`, `action` | `input` (`steer` のみ) | backend task view |

- `request` は MCP tool の `session_id` を除いた引数と同一の typed input とし、
  `orchestration::TaskRequest::parse` がそのまま検証する。executable / raw argv /
  environment block / network policy / scope 外 path / `session_id` 以外の所有情報は
  受け付けない。
- `backend` は `codex` / `opencode` / `devin` / `devin_cloud`。未知の値は dispatch 前に拒否する。
  `task_get` / `task_control` / `task_start` は backend を明示する (backends store が唯一の
  source of truth で、ID から横断探索する第二 index を作らない)。`task_list` は cross-backend
  projection で、各 item が `backend` を持つ。
- 応答の `error` は backend / core の既存 error 文字列をそのまま含む。新しい prefix は
  protocol / routing 層だけが付ける (`TASK_PROTOCOL_INVALID`, `TASK_SESSION_NOT_FOUND`,
  `TASK_SESSION_NOT_ACTIVE`, `TASK_PEER_REJECTED`, `TASK_BACKEND_UNSUPPORTED`)。

例:

```json
{"command":"task_start","protocol_version":3,"session_id":"0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb","backend":"opencode","operation_id":"0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa","request":{"task":"implement api","model":"anthropic/claude-sonnet-4","agent":"build"}}
{"ok":true,"result":{"task_id":"0199cccc-cccc-7ccc-8ccc-cccccccccccc","status":"accepted","revision":1,"generation":0},"error":null}

{"command":"task_control","protocol_version":3,"session_id":"0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb","backend":"devin","task_id":"0199cccc-cccc-7ccc-8ccc-cccccccccccc","operation_id":"0199dddd-dddd-7ddd-8ddd-dddddddddddd","action":"steer","input":"add tests"}
{"ok":true,"result":{"task_id":"0199cccc-cccc-7ccc-8ccc-cccccccccccc","status":"running","revision":3,"generation":2},"error":null}

{"command":"task_list","protocol_version":3,"session_id":"0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb","limit":50}
{"ok":true,"result":{"tasks":[],"backends":{"codex":{"status":"ok","total":0,"skipped":0},"devin_acp":{"status":"unavailable","error":"..."}},"total":0,"truncated":false,"limit":50},"error":null}
```

エラー例:

```json
{"ok":false,"result":null,"error":"TASK_SESSION_NOT_FOUND: session 0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb is not active"}
{"ok":false,"result":null,"error":"TASK_BACKEND_UNSUPPORTED: unsupported task backend \"cursor\""}
{"ok":false,"result":null,"error":"OPERATION_CONFLICT: operation_id was already accepted with a different request"}
```

### 2.6 Operation ID の保存・再利用 (B2 の契約)

- `--operation-id <uuid>` が無い場合、CLI は送信前に UUID を生成して表示し
  (`operation_id: <uuid>`、machine-readable 出力では `operation_id` field)、durable save が
  成功した後でだけ送信する。save 失敗時は送信せず終了する。
- 保存先: `<state_dir>/task-operations/<session_id>/<operation_id>.json`。directory `0700`、
  file `0600`、`create_new` + `fsync` + rename の atomic write。secret を書かない。

```json
{
  "schema_version": 1,
  "operation_id": "0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa",
  "session_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb",
  "backend": "opencode",
  "request_fingerprint": "b9b1...",
  "created_at": 1790000000,
  "task_id": null
}
```

- `request_fingerprint` は typed request の canonical JSON (key 昇順・compact・UTF-8) の
  client-side digest。task 文面 / control input 自体は保存しない。応答成功後は返った
  `task_id` を best-effort で atomic に追記し、`task reconcile` (operation record → `task_get`)
  と再発見に使う。
- 同一 session + 同一 `operation_id` の record があり digest が一致する再送は、そのまま送信して
  backend の receipt replay を受ける。digest 不一致は **送信せず** client 側で conflict として
  拒否する。server 側の同一 ID / 異なる fingerprint 検査 (`OPERATION_CONFLICT`) は最終権威と
  して維持する。
- record の保持は task retention (24h) 以上とし、期限切れ record は opportunistic に prune する。
  operation record は authoritative task store ではなく、backend store と矛盾した場合は
  backend store / `task_get` の結果を優先する。

### 2.7 切断 / lifetime

- 1 request の lifetime は write 1 回 + response read 1 回。既存
  `CONTROL_READ_TIMEOUT` / `CONTROL_WRITE_TIMEOUT` (5s) を上限とする。client が response 受信前に
  切断しても **accepted task を cancel しない**。同じ `operation_id` の再送で receipt replay、
  または `task_list` / `task_get` で再発見する。
- write 途中の切断・不正 JSON は副作用の前に拒否する (受理 receipt は backend が保存順序を
  維持するため、送信済みで応答が無い場合は replay/conflict 判定に従う)。
- client 切断と session 停止を区別する。session 停止は既存の停止契約
  (`session stop` / supervisor shutdown) に従い、task は backend の reconcile で
  `interrupted` / `reconciliation_required` 等として保持し、success / failure に読み替えない。
  task record は retention まで読める。
- supervisor / backend 再起動後の `task_list` / `task_get` は backend store と remote state から
  照合する。不明を成功・失敗と表示せず、根拠なく child を再起動しない。
- CLI は response 待ちの間に supervisor を勝手に再起動しない。connect 失敗は既存の
  `connect_supervisor` エラーで終了し、supervisor を自動起動しない (自動起動は `session start`
  の既存経路だけが行う)。supervisor が停止している場合、active session も存在しないため
  task request は成立しない。

### 2.8 CLI binding (B2 が実装する表面)

```sh
temote-mcp task start   --local --session <id> --backend <b> [--operation-id <uuid>] [backend flags] -- <task>
temote-mcp task list    --local --session <id> [--limit <n>]
temote-mcp task get     --local --session <id> --backend <b> <task-id> [--after-revision <n>]
temote-mcp task control --local --session <id> --backend <b> <task-id> --action steer|resume|interrupt [--operation-id <uuid>] [--input <text>|-- <text>]
temote-mcp task reconcile --local --session <id> [<operation-id>]
```

- `--local` は transport selector のみ。rename 前は現 binary 名 `temote-mcp` で提供する (G)。
- `task reconcile` は wire command を追加せず、保存済み operation record から `task_id` を解決して
  `task_get` を呼ぶ B2 の複合操作とする。record が無い / `task_id` 未確定なら `task_list` を
  表示して不確実性を明示する。
- `task get` / `task control` で `--backend` を省略できるのは、同一 session の `task_list` を
  引いて backend を一意に解決できた場合だけとする。cross-session の探索はしない。
- CLI は task 文面 / control input を stdout / log / operation record に保存しない。
  evidence の read は現行 MCP `evidence_read` の経路に残す (local control 経由の evidence
  transport は後続 packet。B0 の scope 外)。

## 3. Read / change scope (現状確認した実在物)

- `src/session_control.rs`: `ControlRequest` (`command` tag, snake_case)、
  `ControlResponse {ok,result,error}`、`dispatch_request`、`handle_control_connection`、
  `request_at_path`、`CONTROL_PROTOCOL_VERSION = 2`、`run_supervisor` の socket dir `0700` /
  socket `0600`、`local_control()` の厳密一致、`AttachActivityRequest` の `schema_version` 例。
- `src/supervisor.rs`: `SessionSupervisor` の start/stop/restart/instance 解決、
  `approvals::spawn_runtime_with_activity` が session runtime と owner-only session socket を作る。
- `src/approvals.rs`: `local_approval` / `ensure_local_approval_with_activity` /
  `request_approval` と fail-closed console 不在、`ApprovalClass`。
- `src/config.rs`: `state_dir()` (0700)、`socket_path(id)`、`supervisor_socket_path()`、
  `load_session` と `Session` (id / started_at / process_id / cwd / permitted_directories /
  permission_mode / grants)。
- `src/mcp.rs`: `config::load_session` → `orchestration::invoke` の MCP 経路 (同じ core)。
- `src/orchestration.rs` / `orchestration/requests.rs`: typed request と承認経路。
- `src/cli.rs`: `Command` に `task` サブコマンドは無い。

この packet の変更は本ファイルのみ。B1 / B2 はここに固定した契約を実装し、契約を変更する場合は
本ファイルを先に更新する。

## 4. B1 / B2 の実装手順 (この契約の使い方)

**B1:** `ControlRequest` に `Task*` を additive 追加 + `CONTROL_PROTOCOL_VERSION = 3` →
peer credential / `protocol_version` / session / backend を dispatch 前に検証 →
解決済み session を executor へ route → `orchestration::invoke` → bounded response。
fixture backend で socket routing を確認し、認証・scope・version・ownership の拒否経路を
同じ test suite に置く。lifecycle request が v2 client から引き続き使えることを regression で
固定する。

**B2:** CLI parsing → operation ID を保存・表示 → 送信 → task / operation ID と状態を表示。
同じ ID の異なる payload を送信しない。切断時に server を再起動しない。`--json` の有無で
人間向け / machine-readable 出力を切り替える。

## 5. Acceptance (B0 の完了条件)

- [x] protocol version の互換方針と不一致 error が request / response 例で固定されている (2.1)
- [x] 全 task 操作で session 明示、ID からの探索禁止、instance ownership が固定されている (2.2)
- [x] 認証境界・peer 検証・permission context・approval の local 挙動が固定されている (2.3)
- [x] supervisor routing と session 実行境界、拒否経路が固定されている (2.4)
- [x] 4 command の request / response / error / bound 例が揃っている (2.5)
- [x] operation ID の保存先・atomic write・送信順序・指紋検査・保持が固定されている (2.6)
- [x] 切断 / session 停止 / 再起動 / retry の lifetime が固定されている (2.7)
- [x] `--local` が yolo / approval bypass / sessionless にならないことが明記されている (2.1–2.7)

## 6. Validation

- docs-only packet。`git diff --check` と JSON 例の parse 確認を実施する。
- runtime acceptance (socket routing / CLI 動作) は B1 / B2 の範囲であり、この packet に PASS を
  付けない。
- `CONTROL_PROTOCOL_VERSION = 3` への変更、peer credential 検証、`local_control()` の
  `>=` 化は B1 の実装対象で、この packet では未実施。

## 7. Out of scope (次 packet へ残す項目)

- `ControlRequest` / supervisor executor / CLI の実装 (B1 / B2)。
- local control 経由の evidence transport と `evidence_read` の local 対応。
- Windows named pipe transport (初期 scope 外)。
- session / workspace の割当 (C 系)、environment preparation (D 系)、delivery (E 系)。
- operation record の GC 実装と、retention を超える長期 retry の扱い。

## 8. Completion report

packet B0 — 契約書を `issues/open/20260925-b0-local-task-protocol-contract.md` として納品。
コード変更・runtime 検証は未実施 (設計 packet のため)。B1 は §2.1–2.5、B2 は §2.6–2.8 の契約を
そのまま実装入力にできる。残件は §7 の通り。
