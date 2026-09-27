# B0: local task protocol / 認証 scope / 実行所有者の契約

Status: contract delivered (設計 packet。実装コードなし。PR #56)
Packet: B0 (Phase B — local frontend の第 1 child)
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `6df8181` (B0 は A4 PR #55 に依存しない)
Parent issue: `issues/open/20260924-temote-development-harness-restructure.md` (PR #47)
Prerequisites: A1 `51424a4` / A2 `9377bc8` / A3 `3e342d6` (すべて `main` merge 済み)、
`src/session_control.rs` の `CONTROL_PROTOCOL_VERSION = 2` / `MAX_CONTROL_MESSAGE_BYTES = 64 KiB` /
`CONTROL_READ_TIMEOUT = CONTROL_WRITE_TIMEOUT = 5s`、`approvals::SESSION_RESPONSE_TIMEOUT = 2h`、
`config::supervisor_socket_path()`

Updated: 2026-09-25 — PR #56 review 反映 (1): operation record の session instance 束縛、`request` の
flat fixture 化、response lifetime の分離、task frame 上限 (16 MiB)、v2 supervisor の実挙動修正。
Updated: 2026-09-25 — PR #56 review 反映 (2): instance precondition
(`expected_instance_generation`) により Info → task 送信間の再起動競合 (TOCTOU) を server 側で
クローズ。client 側の事前比較は fast path に限定。

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
  - task request は `protocol_version >= 3` を要求する。
  - 既存の `local_control()` の厳密一致チェックは「その client が必要とする最小 version 以上」へ
    変更する。`serve` / `up` は最小 2、task CLI は最小 3、supervisor upgrade の capability 交換は
    引き続き厳密一致 (binary 状態を handoff するため)。
- **v2 supervisor への送信は事前に止める。** task CLI は接続後に `Ping` を送り、
  `control_protocol < 3` なら task request を送らず、**client 生成の error** として報告する:

```text
control protocol 2 does not support Temote task operations (required: 3);
restart the lifecycle supervisor with the matching Temote version
```

  v2 supervisor は未知の `task_*` command を `ControlRequest` の deserialize で拒否し、
  response を書かずに接続を閉じる (現行実装の `invalid control request` 経路)。このとき client が
  観測するのは **EOF / connection reset であり structured error ではない**。B1 はこの挙動を
  regression test に固定し、client は EOF を受理と解釈しない (再 Ping → 上記 client 生成 error)。
- structured な protocol error を返すのは **v3 server** が `protocol_version < 3` の task request を
  受けた場合だけとし、approval / dispatch の前に `TASK_PROTOCOL_INVALID` として返す。
- 未知 field は B1 で task request と nested `request` に限り拒否する。task request は
  `protocol_version`・`session_id`・`backend`・nested `request` の型検証を approval と backend
  dispatch の前に完了する。

### 2.2 Session 識別と ownership

- task 操作は **すべて `session_id` を明示**する。task ID や cwd から session を探索して
  自動選択しない。`--session` もしくは検証済みの接続 context を必須とし、CLI が session を
  省略した場合は usage error で停止する (cwd からの推測をしない)。
- request が instance について運ぶのは `session_id` と server 発行
  `expected_instance_generation` (§2.5) だけとする。`started_at` / `process_id` / scope /
  permission mode を caller に指定させない。
- server は live な session instance (`session_id` + `started_at` + `process_id` + canonical
  scope) を解決し、その `config::Session` を core へ渡す。同じ `session_id` でも restart 後の
  別 instance・別 scope は別 owner であり、先行 instance の task を取得・制御できない。
- server は session instance ごとに **opaque な `instance_generation`** を発行し、`Info` /
  `session_list` の `SessionView` に含める。start / restart / supervisor restore のたびに
  再生成し、同じ instance の間だけ安定する。caller はこの値の意味・値を決められず、echo して
  照合を要求できるだけである。
- backend の task ID と operation receipt は完全な session instance から導出される
  (`task_id_for_operation` 等)。同じ `session_id` の再起動後に同じ `operation_id` を送っても、
  backend では **別 task として新規受理され得る**。この重複起動は §2.5 の instance
  precondition と §2.6 の record 束縛で防ぎ、server は instance 変更を replay と見なさない。
- 他 session の task は存在確認も含めて返さない。ownership 不一致は backend の既存 error
  (`CODEX_TASK_NOT_FOUND: task was not found` 等) をそのまま返し、task の存在を漏らさない。

```json
// request
{"command":"task_get","protocol_version":3,"session_id":"0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb","expected_instance_generation":"3f1c0a9e-0b7d-4a1e-8c2f-6a5b4c3d2e1f","backend":"codex","request":{"task_id":"0199cccc-cccc-7ccc-8ccc-cccccccccccc","after_revision":7}}
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
- **instance precondition は request 受領時と backend dispatch 直前の 2 回照合し、dispatch は
  解決済み instance に束縛する。** server は request 受領時に session instance を解決して
  `instance_generation` を照合し、その runtime handle / 解決済み `config::Session` のまま
  dispatch する (`session_id` から dispatch 時に再解決しない)。approval などで待機した後、
  backend を呼ぶ直前に再照合し、instance が変わっていれば backend を呼ばずに
  `TASK_SESSION_INSTANCE_CHANGED` を返す (approval が先に完了していても、その結果を backend
  受理に進めない)。照合後・dispatch 中に instance が停止した場合も、
  backend の既存 `ensure_current_active_instance` が approval / receipt 受理の前に fail
  closed し、別 instance が旧 request を受理しない。長い approval 待ちの間 session の
  start / stop をブロックしない (transition guard を dispatch 全体に保持しない)。
- session が active でない、または instance を解決できない場合は backend を呼ばず
  `TASK_SESSION_NOT_FOUND` / `TASK_SESSION_NOT_ACTIVE` で拒否する。supervisor が代理で
  task を作らない。

### 2.5 Wire contract

既存 envelope を変更しない: request は 1 行 JSON、response は 1 行
`{"ok":bool,"result":json|null,"error":string|null}`。`result` は対応する MCP tool 応答と
同一 JSON (backend view / A3 の merged task list) で、既存の bound (task_list `limit` 1..=128
default 50、task view の field 集合) を守る。

B1 が追加する command (すべて snake_case):

| command | envelope (routing / precondition) | nested `request` (typed input) | `result` |
| --- | --- | --- | --- |
| `task_start` | `protocol_version`, `session_id`, `expected_instance_generation`, `backend` | MCP `*_task_start` args から `session_id` を除いた同一 object (`operation_id`, `task`, backend 固有 field) | backend task view |
| `task_get` | `protocol_version`, `session_id`, `expected_instance_generation`, `backend` | MCP `*_task_get` args から `session_id` を除いた同一 object (`task_id`, 任意 `after_revision`) | backend task view (`not_modified` 含む) |
| `task_list` | `protocol_version`, `session_id`, `expected_instance_generation` | MCP `*_task_list` args から `session_id` を除いた同一 object (任意 `limit`) | A3 merged `{tasks, backends, total, truncated, limit}` |
| `task_control` | `protocol_version`, `session_id`, `expected_instance_generation`, `backend` | MCP `*_task_control` args から `session_id` を除いた同一 object (`task_id`, `operation_id`, `action`, 任意 `input`) | backend task view |

- すべての task command は envelope に `expected_instance_generation` (server 発行の opaque id) を
  **必須**で含む。欠落・型不一致は `TASK_PROTOCOL_INVALID`。live instance との不一致は
  `TASK_SESSION_INSTANCE_CHANGED` で拒否し、拒否の時点は照合の時点に従う:
  - 受領時の照合で不一致 → **approval の前に**拒否する (approval は要求しない)。
  - approval 待ちの間に instance が変わった場合 → approval 完了後の **dispatch 直前照合**で
    検出し、backend dispatch と receipt 受理の前に拒否する (approval 自体はすでに完了して
    いる。approval 結果を受理や成功に読み替えない)。
  server は §2.4 の 2 回照合 (受領時 / dispatch 直前) で検証する。これは scope 指定権ではなく
  server 発行 identity の照合条件であり、caller は generation 以外の instance field を
  指定できない。client 側の事前比較 (§2.6) は fast path であり、Info と task 送信の間の
  再起動 (TOCTOU) はこの server 側 precondition で閉じる。
- **normalization rule**: server は envelope の field (`session_id` / `backend` /
  `expected_instance_generation`) を nested `request` へ merge しない。parser
  (`orchestration::TaskRequest::parse`) へ渡す入力は nested `request` そのものであり、
  `operation_id` / `task_id` / `action` / `input` は必ず `request` 内にある。envelope の
  `session_id` を `request` にも複製した場合、または envelope 側に typed field を置いた場合は
  `TASK_PROTOCOL_INVALID` として dispatch 前に拒否する。この normalization は exact fixture で
  regression 固定する。
- `request` は executable / raw argv / environment block / network policy / scope 外 path /
  session ownership field を受け付けない (`TaskRequest::parse` の既存検証)。
- `backend` は `codex` / `opencode` / `devin` / `devin_cloud`。未知の値は dispatch 前に拒否する。
  `task_get` / `task_control` / `task_start` は backend を明示する (backends store が唯一の
  source of truth で、ID から横断探索する第二 index を作らない)。`task_list` は cross-backend
  projection で、各 item が `backend` を持つ。
- 応答の `error` は backend / core の既存 error 文字列をそのまま含む。新しい prefix は
  protocol / routing 層だけが付ける (`TASK_PROTOCOL_INVALID`, `TASK_SESSION_NOT_FOUND`,
  `TASK_SESSION_NOT_ACTIVE`, `TASK_SESSION_INSTANCE_CHANGED`, `TASK_PEER_REJECTED`,
  `TASK_BACKEND_UNSUPPORTED`, `TASK_FRAME_TOO_LARGE`)。

exact fixture (B1 の mock / integration test はこの JSON をそのまま使う):

```json
{"command":"task_start","protocol_version":3,"session_id":"0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb","expected_instance_generation":"3f1c0a9e-0b7d-4a1e-8c2f-6a5b4c3d2e1f","backend":"opencode","request":{"operation_id":"0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa","task":"implement api","model":"anthropic/claude-sonnet-4","agent":"build"}}
{"ok":true,"result":{"task_id":"0199cccc-cccc-7ccc-8ccc-cccccccccccc","status":"accepted","revision":1,"generation":0},"error":null}

{"command":"task_control","protocol_version":3,"session_id":"0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb","expected_instance_generation":"3f1c0a9e-0b7d-4a1e-8c2f-6a5b4c3d2e1f","backend":"devin","request":{"task_id":"0199cccc-cccc-7ccc-8ccc-cccccccccccc","operation_id":"0199dddd-dddd-7ddd-8ddd-dddddddddddd","action":"steer","input":"add tests"}}
{"ok":true,"result":{"task_id":"0199cccc-cccc-7ccc-8ccc-cccccccccccc","status":"running","revision":3,"generation":2},"error":null}

{"command":"task_list","protocol_version":3,"session_id":"0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb","expected_instance_generation":"3f1c0a9e-0b7d-4a1e-8c2f-6a5b4c3d2e1f","request":{"limit":50}}
{"ok":true,"result":{"tasks":[],"backends":{"codex":{"status":"ok","total":0,"skipped":0},"devin_acp":{"status":"unavailable","error":"..."}},"total":0,"truncated":false,"limit":50},"error":null}
```

拒否例:

```json
{"ok":false,"result":null,"error":"TASK_PROTOCOL_INVALID: task_session_id must be carried in the envelope, not in request"}
{"ok":false,"result":null,"error":"TASK_SESSION_NOT_FOUND: session 0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb is not active"}
{"ok":false,"result":null,"error":"TASK_SESSION_INSTANCE_CHANGED: expected instance 3f1c0a9e-0b7d-4a1e-8c2f-6a5b4c3d2e1f but the live session instance is 8a2d...; re-resolve the session and use a new operation id for a new attempt"}
{"ok":false,"result":null,"error":"TASK_BACKEND_UNSUPPORTED: unsupported task backend \"cursor\""}
{"ok":false,"result":null,"error":"OPERATION_CONFLICT: operation_id was already accepted with a different request"}
```

競合 regression (B1): `Info` が instance B を返した直後、task request の dispatch 前に session が C へ
再起動する fixture で、request は `TASK_SESSION_INSTANCE_CHANGED` で拒否され、B / C のどちらにも
backend side effect と receipt が無いことを確認する。retry 経路 (`Info` B → 送信直前 C 再起動 →
旧 operation_id 再送) も同じ fixture に含める。

#### Frame bound

- 既存 command の `MAX_CONTROL_MESSAGE_BYTES = 64 KiB` は変更しない。
- task command は **`MAX_TASK_CONTROL_LINE_BYTES = 16 MiB`** を使う。導出:
  - request: `MAX_TASK_INPUT_BYTES = 1 MiB` の task 文面は許容 UTF-8 (NUL のみ禁止) の
    JSON escape で最大約 6 倍になり得るため ≤ 約 6.3 MiB。backend 固有 option と envelope を
    加えても 16 MiB 未満。
  - response: task view は 1 件あたり `MAX_TASK_RECORD_BYTES = 64 KiB` 以下の record から導出され、
    `task_list` limit 最大 128 件で ≤ 約 8.4 MiB。
- B1 は task request / response にこの bound を使い (`encode_line` の 64 KiB 定数を流用しない)、
  request が上限を超える場合は parse / dispatch 前に `TASK_FRAME_TOO_LARGE` で拒否する
  (side effect 0)。client も task response を同じ上限で読む。
- chunking / byte-bounded pagination はこの導出が成立する限り導入しない。上限を超える field を
  追加する packet は bound を再導出し、暗黙に超過させない。

### 2.6 Operation ID の保存・再利用 (B2 の契約)

- 保存先: `<state_dir>/task-operations/<session_id>/<operation_id>.json`。directory `0700`、
  file `0600`、`create_new` + `fsync` + rename の atomic write。secret を書かない。
- **送信前に live session instance を解決して record に束縛する。** `session_info` / `Info` の
  `SessionView` が返す server 発行 `instance_generation` を保存する。`started_at` /
  `process_id` / canonical `cwd` は診断情報として併記してよい。task ID と operation receipt が
  完全な instance から導出されるため、record は `session_id` だけでは不足する。

```json
{
  "schema_version": 1,
  "operation_id": "0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa",
  "session_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb",
  "session_instance": {
    "session_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb",
    "instance_generation": "3f1c0a9e-0b7d-4a1e-8c2f-6a5b4c3d2e1f",
    "started_at": 1790000000,
    "process_id": 4242,
    "canonical_scope": "/home/user/src/example"
  },
  "backend": "opencode",
  "request_fingerprint": "b9b1...",
  "created_at": 1790000000,
  "task_id": null
}
```

- 保存順序: `Info` で instance 解決 (session が active でなければ送信前に拒否) → typed request の
  canonical JSON (key 昇順・compact・UTF-8) から client-side `request_fingerprint` を計算 →
  file を atomic に保存 → 成功した場合だけ `operation_id` を表示し、送信する。save 失敗時は
  送信しない。task 文面 / control input 自体は保存しない。
- 再利用時は **同じ operation_id の record の `instance_generation` と、いま `Info` で解決した
  live instance が返す generation が一致することを要求**する。不一致 (restart / supervisor
  restore) は送信せず
  `operation record belongs to a previous session instance; use a new --operation-id` で拒否する。
  この事前比較は fast path であり、Info と task 送信の間の再起動競合は §2.5 の
  `expected_instance_generation` precondition を server が dispatch 直前に照合することで閉じる。
- 同一 instance + 同一 operation_id + digest 一致の再送はそのまま送信し、backend の receipt
  replay を受ける。digest 不一致は送信せず client 側 conflict として拒否する。server 側の
  同一 ID / 異なる fingerprint 検査 (`OPERATION_CONFLICT`) は最終権威として維持する。
- instance generation が変わった operation は replay ではなく新規試行である。旧 operation の
  receipt は instance をまたいで成立しないため、fresh な `operation_id` を要求する。
- 応答成功後は返った `task_id` を record に best-effort で atomic 追記し、`task reconcile` と
  再発見に使う。instance が変わった record の `task_id` を現在 instance の照合に使わない。
- record の保持は task retention (24h) 以上とし、期限切れ record は opportunistic に prune する。
  operation record は authoritative task store ではなく、backend store と矛盾した場合は
  backend store / `task_get` の結果を優先する。

### 2.7 Lifetime と timeout

- **ingestion と response wait を分離する。**
  - `CONTROL_READ_TIMEOUT` / `CONTROL_WRITE_TIMEOUT` (5s) は接続、request line の転送、
    server の request 受信 (parse 前) の上限であり、dispatch の上限ではない。
  - server の dispatch は `ask` の approval console 応答 (`SESSION_RESPONSE_TIMEOUT = 2h`) と
    backend の child RPC (codex `RPC_TIMEOUT = 30s` 等) / session 確立を待ち得る。response は
    dispatch 完了後に 1 行で返し、5s の control write timeout を適用しない。
  - client の task response wait は **`TASK_RESPONSE_WAIT_TIMEOUT`** とし、少なくとも
    `SESSION_RESPONSE_TIMEOUT` (2h) + backend 確立 budget (5m) 以上にする。B1/B2 が定数を
    実装し、本契約の 5s を response に流用しない。
- 1 request の lifetime は上記 wait までの write 1 回 + response read 1 回。client が response
  受信前に切断・Ctrl-C・timeout しても **accepted task を cancel しない**。operation_id は送信前に
  表示・保存済みなので、同じ ID の再送で receipt replay、または `task_list` / `task_get` /
  `task reconcile` で再発見する。client は timeout を成功・失敗と表示せず
  `unknown; task may have been accepted` と operation_id / reconcile 手順を表示する。
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
- `--operation-id` が無い場合、CLI は `Info` で instance を解決した後に `instance_generation` を
  record へ保存し、UUID を生成、record 保存後に `operation_id: <uuid>` を表示してから送信する。
  すべての task command は record / 直前の `Info` が返した `instance_generation` を envelope の
  `expected_instance_generation` に echo する。
- `task reconcile` は wire command を追加せず、保存済み operation record から `task_id` を解決して
  `task_get` を呼ぶ B2 の複合操作とする。record が無い / instance 不一致 / `task_id` 未確定なら
  `task_list` を表示して不確実性を明示する。
- `task get` / `task control` で `--backend` を省略できるのは、同一 session の `task_list` を
  引いて backend を一意に解決できた場合だけとする。cross-session の探索はしない。
- CLI は task 文面 / control input を stdout / log / operation record に保存しない。
  evidence の read は現行 MCP `evidence_read` の経路に残す (local control 経由の evidence
  transport は後続 packet。B0 の scope 外)。

## 3. Read / change scope (現状確認した実在物)

- `src/session_control.rs`: `ControlRequest` (`command` tag, snake_case)、
  `ControlResponse {ok,result,error}`、`dispatch_request`、`handle_control_connection`、
  `request_at_path`、`CONTROL_PROTOCOL_VERSION = 2`、`MAX_CONTROL_MESSAGE_BYTES = 64 KiB`
  (`encode_line` / `read_control_request` / `read_line_limited`)、`CONTROL_READ_TIMEOUT` /
  `CONTROL_WRITE_TIMEOUT = 5s`、`run_supervisor` の socket dir `0700` / socket `0600`、
  `local_control()` の厳密一致、`SessionView` (`id` / `started_at` / `process_id` / `cwd`)、
  `AttachActivityRequest` の `schema_version` 例。
- `src/supervisor.rs`: `SessionSupervisor` の start/stop/restart/instance 解決、
  `approvals::spawn_runtime_with_activity` が session runtime と owner-only session socket を作る。
- `src/approvals.rs`: `local_approval` / `ensure_local_approval_with_activity` /
  `request_approval` と fail-closed console 不在、`ApprovalClass`、`SESSION_RESPONSE_TIMEOUT = 2h`。
- `src/config.rs`: `state_dir()` (0700)、`socket_path(id)`、`supervisor_socket_path()`、
  `load_session` と `Session` (id / started_at / process_id / cwd / permitted_directories /
  permission_mode / grants)。
- `src/mcp.rs`: `config::load_session` → `orchestration::invoke` の MCP 経路 (同じ core)。
- `src/orchestration.rs` / `orchestration/requests.rs`: typed request と承認経路
  (`MAX_TASK_INPUT_BYTES = 1 MiB`、task_list limit 1..=128)。
- backend modules: `MAX_TASK_RECORD_BYTES = 64 KiB`、`RPC_TIMEOUT = 30s` (Codex) 等、
  `task_id_for_operation` の instance 依存導出。
- `src/cli.rs`: `Command` に `task` サブコマンドは無い。

この packet の変更は本ファイルのみ。B1 / B2 はここに固定した契約を実装し、契約を変更する場合は
本ファイルを先に更新する。

## 4. B1 / B2 の実装手順 (この契約の使い方)

**B1:** `ControlRequest` に `Task*` を additive 追加 + `CONTROL_PROTOCOL_VERSION = 3` →
session runtime に opaque `instance_generation` を発行し `SessionView` へ追加 →
task frame bound (16 MiB) と response timeout 分離 → peer credential / `protocol_version` /
session / `expected_instance_generation` / backend / nested `request` normalization を dispatch
前に検証 → precondition を受領時と dispatch 直前の 2 回照合し、dispatch は解決済み instance の
executor 経由で `orchestration::invoke` → bounded response。
fixture backend で socket routing を確認し、v2 supervisor の EOF 挙動、認証・scope・version・
ownership・instance precondition・frame 超過の拒否経路を同じ test suite に置く。特に
「`Info` が instance B を返した直後、dispatch 前に C へ再起動」と、その状態での旧 operation_id
retry の競合 fixture を必須とし、B / C どちらにも backend dispatch と receipt が無いことを
固定する。lifecycle request が v2 client から引き続き使えることを regression で固定する。

**B2:** CLI parsing → `Info` で instance 解決 → operation ID を保存・表示 → 送信 →
task / operation ID と状態を表示。instance 不一致の record は送信しない。同じ ID の異なる
payload を送信しない。切断 / timeout 時に server を再起動せず、`unknown` と reconcile 手順を
表示する。`--json` の有無で人間向け / machine-readable 出力を切り替える。

## 5. Acceptance (B0 の完了条件)

- [x] protocol version の互換方針、client 生成 error、v2 supervisor の EOF 挙動が例で固定されている (2.1)
- [x] 全 task 操作で session 明示、ID からの探索禁止、instance ownership が固定されている (2.2)
- [x] 認証境界・peer 検証・permission context・approval の local 挙動が固定されている (2.3)
- [x] supervisor routing と session 実行境界、拒否経路が固定されている (2.4)
- [x] 4 command の envelope / nested `request` / normalization / exact fixture / error が揃っている (2.5)
- [x] task frame 上限 (16 MiB) の導出と超過時の拒否が固定されている (2.5)
- [x] operation record の session instance 束縛と再起動時の重複起動防止が固定されている (2.6)
- [x] instance precondition (`expected_instance_generation`) を受領時と dispatch 直前に照合して
      Info→送信間の再起動競合 (TOCTOU) を server 側で拒否し、client 事前比較を fast path に
      限定している (2.2 / 2.4 / 2.5 / 2.6)
- [x] ingestion timeout と response wait の分離、切断 / session 停止 / 再起動 / retry が固定されている (2.7)
- [x] `--local` が yolo / approval bypass / sessionless にならないことが明記されている (2.1–2.7)

## 6. Validation

- docs-only packet。`git diff --check` と JSON 例の parse 確認を実施する。
- runtime acceptance (socket routing / CLI 動作) は B1 / B2 の範囲であり、この packet に PASS を
  付けない。
- `CONTROL_PROTOCOL_VERSION = 3` への変更、peer credential 検証、`local_control()` の
  `>=` 化、task frame / timeout の実装は B1 の実装対象で、この packet では未実施。

## 7. Out of scope (次 packet へ残す項目)

- `ControlRequest` / supervisor executor / CLI の実装 (B1 / B2)。
- local control 経由の evidence transport と `evidence_read` の local 対応。
- Windows named pipe transport (初期 scope 外)。
- session / workspace の割当 (C 系)、environment preparation (D 系)、delivery (E 系)。
- operation record の GC 実装と、retention を超える長期 retry の扱い。

## 8. Completion report

packet B0 — 契約書を `issues/open/20260925-b0-local-task-protocol-contract.md` として納品 (PR #56)。
コード変更・runtime 検証は未実施 (設計 packet のため)。B1 は §2.1–2.5、B2 は §2.6–2.8 の契約を
そのまま実装入力にできる。

PR #56 review 反映 1 回目 (2026-09-25):

- operation record を完全な session instance (`started_at` / `process_id` / canonical scope) に
  束縛し、instance 不一致の再送を client 側で拒否する契約にした (§2.2 / §2.6)。
- `request` を MCP args と同一の flat object とし、envelope を routing のみに限定。merge 禁止と
  exact fixture を追加した (§2.5)。
- ingestion (5s) と dispatch response wait (2h approval + backend budget) を分離した (§2.7)。
- task frame 上限 16 MiB を導出し、超過拒否と response への適用を固定した (§2.5)。
- v2 supervisor は structured error を返さず EOF / connection close になることを明記し、
  structured error は v3 server の `protocol_version < 3` 拒否時とする (§2.1)。

PR #56 review 反映 2 回目 (2026-09-25):

- Info と task 送信の間の再起動競合 (TOCTOU) を解消した。server 発行の opaque
  `instance_generation` を `SessionView` に追加し、すべての task command の envelope に
  `expected_instance_generation` を必須 precondition として含める。server は request 受領時と
  backend dispatch 直前の 2 回照合し、不一致は `TASK_SESSION_INSTANCE_CHANGED` で拒否する。
  受領時の不一致は approval の前に、approval 待ちの間の変更は approval 完了後の dispatch 直前
  照合で検出し、backend dispatch / receipt 受理の前に拒否する。client 側の事前比較は fast path
  とし、retry は instance が変わった時点で新規試行として fresh な operation_id を要求する
  (§2.2 / §2.4 / §2.5 / §2.6)。
- B1 の必須 regression に「Info が B を返した直後の C 再起動」と旧 operation_id retry の
  競合 fixture (backend dispatch 0 回) を追加した (§2.5 / §4)。

PR #56 review 反映 3 回目 (2026-09-25):

- approval timing の nit を反映。受領時の不一致は **approval の前**に拒否し、approval 待ちの
  間に instance が変わった場合は approval 完了後の **dispatch 直前照合**で検出して backend
  dispatch / receipt 受理の前に拒否する、と §2.4 / §2.5 で時点を明記した (approval 結果を
  受理や成功に読み替えない)。
