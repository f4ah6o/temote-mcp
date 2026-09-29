# Fabric Dashboard: Zero Trust 配下の `/dash` Web dashboard

Status: open / implemented locally; deployed acceptance blocked
Repository: `f4ah6o/temote-mcp`
Priority: P1 operator visibility
Created: 2026-09-29 (Asia/Tokyo)
Updated: 2026-09-29
Model: gpt-6-sol
Branch: feat/fabric-web-dashboard
PR: https://github.com/f4ah6o/temote-mcp/pull/88 (Draft; deployed acceptance blocked)
Umbrella: `issues/open/20260924-temote-development-harness-restructure.md`
Fabric boundary: `issues/open/20260926-temote-fabric-product-boundary.md`
Observation plane: `issues/open/20260926-cloud-observation-knowledge-plane.md`
Related local viewer: `issues/done/20260914-local-activity-viewer.md`

## 1. Decision

Temote Fabric と **同じ Cloudflare Worker deployment** に read-only Web dashboard を追加する。

公開 surface は既存 Fabric hostname の:

```text
/dash
/dash/*
```

とし、別 Worker / Pages project / 別 public hostname は作らない。

Dashboard は **Cloudflare Zero Trust / Access の内側**に置く。
同じ hostname / Worker を保護している Access application をそのまま利用し、`/dash` だけを公開 bypass しない。

初期版は **read-only** とする。
session stop/restart、task control、approval answer、permission mutation、delivery 等の write 操作はこの issue に含めない。

## 2. Goal

ブラウザから、Temote Fabric がすでに持つ host / session / task / context / observation の状態を一つの画面で確認できるようにする。

最低限、次を確認できること。

- Fabric deployment identity / version / public contract fingerprint
- federated host 一覧と online / offline / unknown / unavailable
- host ごとの live session 一覧
- session の lifecycle / permission mode / workspace identity
- session ごとの delegated task 一覧
- backend / task state / pending interaction の有無
- context freshness / unresolved items / current workspace/task projection
- Fabric に replicate 済みの sanitized observation を使った recent timeline
- live authority と replicated observation を明確に区別した freshness 表示

Dashboard 自体を新しい execution authority にしない。

## 3. Why this is a separate issue

既存 local activity viewer は intentionally:

- owner-only local CLI
- best-effort in-memory history
- read-only
- Web UI / public HTTP / gateway exposure を対象外

として実装された。

Fabric dashboard では local activity stream をそのまま remote 公開しない。
cloud 側の表示には Fabric observation plane の **sanitized replicated observation** と、既存の bounded read projection を使う。

つまり:

```text
local activity viewer
  = local operator diagnostic

Fabric Dashboard
  = authenticated remote read projection
```

であり、責務を分離する。

## 4. Product / authority boundary

Dashboard は presentation projection であり、authoritative state store を持たない。

### Host authoritative

次は Temote Host の live state が authority:

- backend process が running か
- session lifecycle
- task current state
- pending approval / interaction
- workspace current state
- verification result
- permission / approval state

### Fabric replicated

次は Fabric/D1 の durable replica / derived projection:

- sanitized observation
- replication revision / gap
- context / knowledge projection
- last synchronized state

host が offline の場合、replicated state を `running` 等の current state として表示してはならない。

UI / API は state と一緒に最低限:

```text
authority = host_live | fabric_replica | unavailable
freshness
source_revision?
observed_at?
gap?
```

を返し、画面上でも区別する。

## 5. Route design

### Browser assets

```text
GET /dash          -> authenticated 308 -> /dash/
GET /dash/         -> dashboard shell
GET /dash/app.js
GET /dash/styles.css
```

初期版は history API routing を持つ SPA にせず、選択状態は URL query/hash で保持する。
未知の `/dash/*` を無制限に index.html へ fallback させない。

### Dashboard read API

同一 origin に presentation-only API を追加する。

```text
GET /dash/api/v1/bootstrap
GET /dash/api/v1/hosts
GET /dash/api/v1/hosts/:host_id/sessions
GET /dash/api/v1/hosts/:host_id/sessions/:session_id
GET /dash/api/v1/hosts/:host_id/sessions/:session_id/tasks
GET /dash/api/v1/hosts/:host_id/sessions/:session_id/context
GET /dash/api/v1/hosts/:host_id/sessions/:session_id/timeline?after=&limit=
```

この API は public MCP contract ではない。
`gateway/contract/routed-tools.json` と別の second execution API を作るものでもない。

handler は existing routing / observation / context core を直接再利用し、
Worker 自身の `/mcp` を HTTP self-call しない。

API は GET のみ。
初期版では POST / PUT / PATCH / DELETE を追加しない。

## 6. Zero Trust / authentication contract

### Edge

`/dash` と `/dash/*` は既存 Fabric Worker / hostname の Cloudflare Access policy の内側に置く。

- public bypass を追加しない
- query token を使わない
- dashboard 独自 password / session store を作らない
- browser login は Cloudflare Access に委ねる

### Worker defense in depth

現行 Fabric は `/mcp` を `authorizeClient(request, env)` で認証している。
この関数は検証済み Access JWT の前に `Bearer ${env.CLIENT_TOKEN}` の一致を受理するため、
そのまま再利用すると Access を通らない到達経路でも `CLIENT_TOKEN` だけで
dashboard に到達できる経路が残る。
これは本 dashboard が掲げる二段認証契約と一致しない。

Dashboard では **検証済み Access JWT を必須**とする。
再利用するのは `access.js` の Access JWT 検証ロジック
(`cf-access-jwt-assertion` の shape 検証、JWKS signature、issuer、audience、
expiry/nbf、subject、`ACCESS_ALLOWED_EMAILS` の確認)だけであり、
`CLIENT_TOKEN` fallback を含む `authorizeClient()` 全体の動作ではない。

```text
Access edge auth
        +
Worker-side verified Access JWT (dashboard auth mode)
```

の二段を維持する。

D0 prerequisite として、dashboard request にだけ適用する
Access-only 認証入口(明示的な auth mode)を `access.js` に追加する。
完成した API の signature はここでは固定せず、契約だけを定める:

- `CLIENT_TOKEN`、`HOST_TOKEN`、`HOST_TOKENS_JSON` の各 host token は
  dashboard 認証の代替にならない。
- shared env の `CLIENT_TOKEN` を一時的に無効化する実装はしない。
- signature / issuer / audience / expiry / subject / `ACCESS_ALLOWED_EMAILS` の
  検証を省略しない。
- header の存在、cookie の存在、JWT の decode だけをもって認証済みにしない。
- `/mcp` は従来どおり `authorizeClient()` の動作を維持し、
  `CLIENT_TOKEN` compatibility を壊さない。

Cloudflare Workers の `ctx.access` への移行はこの issue の成立条件にしない。
Static Assets の内部 assets router は `ctx.access` を user Worker に渡さないため、
Access identity は引き続き request header の JWT 検証で確立する。
現行 request/JWT validator を捨てる変更は別 packet とする。

`/dash` asset request も Worker-first (`assets.run_worker_first`) で処理して
authorization 後に assets binding へ渡し、
direct-origin / route misconfiguration でも dashboard asset を無認証配信しない。
`/dash`、`/dash/`、JS/CSS、dashboard API、未知の `/dash/*` path のいずれも、
認証完了前に dashboard content を返さない。

実装上の制約として、現在の gateway は global OPTIONS handler と
全応答へ `access-control-allow-origin: *` を付与する `withCors()` を持つ。
dashboard の success / auth failure / exception / 404 / 405 response を
この permissive CORS wrapper に無条件で流さない。
D0 は dashboard-scoped の response path(no-store、same-origin、
§11 の security headers)を route 単位で分離する前提とする。
この issue では runtime code を変更しない。

## 7. Worker / Static Assets deployment

別 deployment を作らず、`gateway/wrangler.toml` の現在の Worker に Cloudflare Workers Static Assets binding を追加する。

想定 layout:

```text
gateway/
  assets/
    dash/
      index.html
      app.js
      styles.css
  src/
    dashboard/
      api.js
      projection.js
  test/
    dashboard-api.test.mjs
    dashboard-security.test.mjs
```

v1 は framework / CDN runtime を必須にしない。
plain HTML + CSS + ES modules で開始し、既存 gateway の Node/Wrangler toolchain だけで deploy できるようにする。

Static Assets は binding 経由で serving し、`/dash` と `/dash/*` は
Worker を先に通す(`assets.run_worker_first`)。
`/mcp`, `/healthz`, host API, observation sync の routing semantics を変更しない。

## 8. Read model

### 8.1 Bootstrap

`GET /dash/api/v1/bootstrap`

minimum:

```json
{
  "service": "temote-fabric",
  "deployment": "...",
  "contract_fingerprint": "...",
  "authenticated": true,
  "generated_at": "...",
  "refresh": {
    "foreground_ms": 5000,
    "background_ms": 30000
  }
}
```

credential / JWT / Access cookie は返さない。

### 8.2 Hosts

Host 表示は **inventory (server-side host membership)** と
**liveness (現在の接続状態)** を分離する。

`readOnlineHosts` の出力は online hosts と probe 不能な ID のみであり、
durable な offline host 一覧ではない
(registry `listCollection` は read 時に期限切れ entry を削除し、
status probe が 404 の host は結果に残らない)。
よって `readOnlineHosts` は liveness overlay としてのみ使い、
offline inventory の唯一の source にしない。

採用する inventory source と merge 規則を次で確定する:

1. **configured host membership** — 認可対象 host 集合は
   `HOST_TOKENS_JSON` の検証済み own-property key とする。
   token 値は response / HTML / log に一切出さない。
   key は `validateHostId` で検証する。
   認証設定を dashboard 用に変更しない。
2. **liveness overlay** — `readOnlineHosts` の registry 現在状態を
   認可対象集合に重ねる。live routing registry の期限切れ削除は止めない。
3. **replica metadata** — owner-scoped `observation_sources` から
   host_id 単位の `last_synced_at` / `journal_degraded` / `gap_count` を
   merge する。`owner_id` は trusted server configuration
   (`OBSERVATION_OWNER_ID`)と authorization mapping から決定し、
   任意の browser parameter で owner を選ばせない。
   replica read は `owner_id + host_id + session_id` で scope し、
   replica の状態を live execution state に昇格させない。
   設定から削除された host を古い D1 row だけで一覧に再公開しない:
   inventory は常に configured membership に限定し、
   replica metadata は membership 内 host の freshness 装飾にのみ使う。

別の durable catalog が必要になる場合は、その必要性、書込み元、保持期間、
削除条件、D1 より先に必要な実装 dependency を個別に設計する。
この issue では新しい durable catalog を追加しない。

`availability` は「Fabric に現在利用可能な live route があるか」だけを
表し、過去に接続した事実や backend process の生存まで意味に含めない。
host ごとの availability を次で区別する:

```text
online    完全な registry 読み取りと status probe が成功し、
          対象 host の live route を確認できた
offline   registry 読み取りが正常・完全だが、対象 host に
          有効な live route がない
          (「実機が停止している」「切断時刻を確認した」という意味には
          しない。「接続履歴なし」「切断理由」も推定しない)
unknown   registry 読み取り失敗、対象 probe 失敗、または結果の
          完全性を確認できない (component failure を明示する)
```

- unknown を offline と断定しない。offline を一覧から消して済ませない。
- observation がないことを「host が存在しない」と同一視しない。
- D1 が失敗しても live route の確認に成功した host は `online` のまま、
  replica metadata component を `unavailable` として表示する (§12)。
- 認可対象から外れた host は inventory に含めない。

`configured_only` は availability state ではなく、host entry の
**情報源属性** (`evidence`)として表示する:
「現在参照できる根拠が configured membership だけ」であることを意味し、
「過去に接続したことがない」という意味は持たせない。

接続履歴と同期履歴は availability と分離し、観測できた入力だけを表示する:

- 証拠が存在する
  → 観測できた履歴を表示する (registry entry が存在する間の
    `last_seen` / `connected_at`、および
    `observation_sources.last_synced_at` の "last synchronized")。
- 証拠が存在しない
  → 過去の接続有無は `unknown` とし、「接続したことがない」と断定しない
    (registry entry は lease expiry で entry 全体が削除されるため、
    prune 後は接続履歴を復元できない)。
- 履歴取得元が失敗した
  → `unavailable` とする。
- `last_synced_at` は最後に observation を同期した時刻であり、
  切断時刻や last_seen ではない。時刻を捏造しない。

欠落・破損した membership 設定や不完全な registry 結果を、
正常な空集合として扱わない。membership の parse/validate 失敗は
component-level `unavailable`、registry 結果の完全性が確認できない
場合は `unknown` とする。
configured membership による認可対象の制限は host 一覧だけでなく
`/dash/api/v1/hosts/:host_id/...` の直接参照にも一貫して適用し、
membership に含まれない `host_id` への直接アクセスを拒否する。

表示:

- host_id
- availability (上記 live-route state)
- evidence 属性 (`configured_only` 等)
- 観測できた接続 / 同期履歴 (connected_at / last_seen / last synchronized) —
  証拠がなければ `unknown`
- protocol/capability summary if already present in bounded host projection
- session count if confirmed
- observation sync freshness

host discovery failure は空配列に変換しない。
`unavailable` を明示する。

### 8.3 Sessions

existing host-aware session listing / info の bounded projection を再利用する。

表示候補:

- session_id
- lifecycle
- permission mode
- workspace type
- repository
- branch/bookmark when available
- started_at
- last error summary if already part of safe projection

raw credential、environment、prompt、command output を追加取得しない。

### 8.4 Tasks

source は `task_list` projection と、D2 で追加する
Host-side `pending_interaction` summary の合成とする。

現行の `compact_task_list_view` は各 task を
backend / task_id / status / revision / last_updated_at に縮約し、
`task_list` (`orchestration.rs` → 各 backend の `task_list_with_store`) は
retained record の read で、runtime に触れず record も変更しない。
一方 `task_get` (`task_get_with_store_and_binary`) は runtime の
確保・再確立と reconciliation を行い得て、その後に pending interactions を
取得する。
つまり現行 list に summary field を足すだけでは値の更新元がなく、
full `task_get` の流用は read-only list に runtime 確保・reconciliation を
持ち込む。欠落 field を `count=0` / `pending=false` に変換しない。

D2 prerequisite として、summary の **生成・更新経路** と **読み取り経路** を
分離した契約を定義する(完成した API ではなく前提契約であり、
observer も保存 field も現状は存在しないものとして設計する):

```text
local runtime を持つ backend (Codex / OpenCode / Devin ACP):
  既存 local runtime owner
    → backend event / scoped read
    → bounded summary

Devin Cloud (local runtime が存在しない):
  D2 で追加する Host-side read-only metadata observer
    → 既存 remote session の status read
    → bounded summary

共通:
  task_list
    → 保存済み summary の projection のみ
    → dashboard
```

境界:

- **更新主体**は producer の種類ごとに定義する
  (`producer_kind`、後述の新規設計 field):
  - `runtime_owner` (Codex / OpenCode / Devin ACP):
    対象 task の runtime を現在所有している既存 owner。
  - `host_remote_observer` (Devin Cloud):
    D2 で追加する Host-side read-only metadata observer(後述)。
    Cloud task に local runtime / runtime owner は存在せず、
    runtime generation や PID を観測所有権の代用にしない。
- dashboard の GET、`task_list`、page load を契機に task/runtime の
  新規起動・再開・再接続を行わない。`ensure_runtime*`、full `task_get`、
  task control を metadata 取得の代用にしない。
- **取得元**は backend ごとの safe な scoped read / event path のみ(下表)。
  raw detail を取得してから隠す方式にせず、summary だけを生成する。
- **保存先**は既存 task ownership / retention / GC に従う
  Host-side metadata とする。owner 検証を省いた別ストアや
  新しい execution authority を作らない。
- **reader**は `task_list` の additive projection のみとし、
  list は保存済み summary の読み取りに留め、取得を合成しない。
- 追加 MCP tool、任意 tool-name を受ける dashboard proxy、
  新しい task control 経路は作らない。
- 既存 field と入力契約を維持し、
  public MCP contract fingerprint を不必要に変更しない。
- task owner / session / workspace の既存検証を迂回しない。

summary が表現する内容:

```text
state              = none | pending | unknown | unsupported | unavailable
count?             bounded integer (source が件数を確定できる場合のみ)
types?             allow-listed 種別の bounded 配列
summary_revision   summary 内容 (state / count / types / truncated)
                   が変化した書込みで増加する独立 revision
                   (`observed_at` のみの更新では増やさない)
observed_at        backend を実際に観測した時刻
producer_kind      = runtime_owner | host_remote_observer
producer_epoch     その producer の所有権世代
                   (runtime_owner は runtime generation、
                   host_remote_observer は observer owner epoch)
expires_at         observed_at + TTL
truncated?         上限に達したことを示す marker
```

- `none` の根拠は backend ごとに定義する(下表): OpenCode は
  permission と question の対象範囲を正常に読み切れた場合、
  Codex / Devin ACP は owner が outstanding request なしを確認した場合、
  Devin Cloud は status read 成功かつ pending signal なしを検証した場合。
- 一部 endpoint の取得失敗、上限超過、owner 不明、runtime 再接続中は
  `unavailable` / `unknown` とし、`none` にしない。
- `expires_at` を過ぎた summary と observer 停止後の値は `unavailable`
  とし、現在の「対話待ちなし」と表示しない。
- field を返さない旧 Host は `unsupported` とし、
  「対話待ちなし」と表示しない。
- 該当 backend の取得失敗は他 task / 他 backend に影響させない。
- 全 backend を恒久的に `unknown` / `unsupported` にするだけで
  pending interaction 機能が完成した扱いにはしない。

freshness / generation:

- `summary_revision` は task `revision` / task status とは独立に増加し、
  summary だけが変化した場合にも UI が更新を検出できる。
- event-driven producer (Codex / Devin ACP) は observer interval ごとに
  `observed_at` を再記述し、owner が生存し最後に確認した pending 状態が
  継続していることを再確認する(backend への再取得ではなく、
  所有中 runtime の in-process 状態による heartbeat)。
  heartbeat が止まった summary は `expires_at` で `unavailable` になるため、
  長時間 pending の request で marker が消えない。
  なお process restart 後に保存済み summary が残っていても、
  メモリ上の pending 状態や接続の完全性まで復元できたとは扱わず、
  現在の owner・接続状態・鮮度が確認できるまで
  古い値を heartbeat だけで fresh に戻さない。
- `observed_at` は backend 観測時刻であり、HTTP response 生成時刻や
  host 接続確認時刻で古い summary を fresh にしない。
  `host_remote_observer` は remote fetch 成功 + 状態検証時のみ
  `observed_at` を更新し、observer process の生存だけで
  remote 状態の鮮度を延長しない (heartbeat 対象外)。
- summary は `producer_kind` + `producer_epoch` を持ち、
  その producer の所有権世代が変わった後の遅延結果による
  上書きを受理しない。

本 issue で確定する bounds:

- observer interval: 5 秒 (producer が対象 task の観測を登録している間のみ;
  dashboard foreground polling と同周期)
- backend scoped read timeout: 2 秒 / endpoint
- 同時実行: runtime あたり 1、host 全体で 4 並行まで
- summary TTL (`expires_at`): `observed_at` + 30 秒
- pending 件数 / `count` 上限: 64
  (OpenCode の既存 `MAX_PENDING_INTERACTIONS` に一致;他 backend も同じ上限)
- `types` 配列: allow-list は `permission` / `question` / `approval`、
  最大 4 種類
- serialized summary 上限: 4 KiB

backend ごとの support 判定と取得元(実ファイル確認済み):
表の `supported` は「D2 で実装する v1 対応目標」であり、
「現在実装済み」の意味ではない。

| backend | v1 support | safe metadata の実際の取得元 | 更新主体・保存先 | freshness / generation | 取得不能・未対応時 | 追加する prerequisite |
|---|---|---|---|---|---|---|
| OpenCode | supported | 既存 `pending_interactions()` の scoped read: `session_list` + `permission_list` + `question_list` を対象 OpenCode session と descendant session に絞り込む (`MAX_PENDING_INTERACTIONS` = 64) | `runtime_owner` producer: 所有中 runtime に対する定期 scoped read → task metadata | `observed_at` + `producer_epoch` | `unavailable` | `task_list` read から分離された observer と summary 保存 field |
| Codex (app-server) | supported | 既存: runtime JSON-RPC channel の approval request event(`mark_waiting_approval` が `waiting_approval` を record に記録する経路) | `runtime_owner` producer: D2 新規に同じ owner が sanitized summary を生成・永続化 + interval heartbeat で `observed_at` を更新 | `observed_at` + `producer_epoch` | `unavailable` | summary field 追加 |
| Devin ACP | supported | 既存: `session/request_permission` event path と `AcpShared.pending_permissions` のメモリ上カウンタ、`mark_task_waiting_approval` による task status 更新 | `runtime_owner` producer: D2 新規に同じ owner が sanitized summary を生成・永続化 + interval heartbeat で `observed_at` を更新 | `observed_at` + `producer_epoch` | `unavailable` | summary field 追加のみ。`pending_permissions` は永続化済みではない |
| Devin Cloud | supported | 既存 `CloudApi::get_session` の bounded status read(`status` / `status_detail`; 既存 reconcile が `waiting_for_approval` → `WaitingApproval` に導出する値) | `host_remote_observer` producer: D2 で追加する Host-side read-only metadata observer(後述)の定期 remote status read → task metadata | `observed_at` + `producer_epoch` | `unavailable` / `unknown` | local runtime owner は存在しない。observer 本体、summary field、観測所有権 field を追加 |

- `types` は `permission` / `question` (OpenCode) と `approval`
  (Codex / Devin ACP / Devin Cloud) を v1 で正しく表示する。
- Devin Cloud の `waiting_for_user` は retained `WaitingInput` status として
  task status に表示し、回答必須の対話と混同しないため
  v1 の pending summary には含めない。
- OpenCode の `interaction_detail` (question 本文・choices、
  permission action/resources)、Codex / Devin ACP の approval detail、
  raw request / result は summary、保存先、Worker response、browser、
  log に出さない。summary は kind / count / freshness のみ。
- OpenCode observer は runtime を所有していない状態で scoped read を
  実行しない(ensure / spawn しない)。所有喪失後は取得を止め、
  summary は期限切れで `unavailable` になる。
- host 側に summary field を持たない旧 Host は `unsupported` とする。

#### Devin Cloud observer (D2 prerequisite)

`devin_cloud.rs` は local に何も実行せず、runtime lease を持たない
(remote session が authority で、`task_get` ごとに reconcile する構造)。
したがって Cloud の producer は runtime owner ではなく、
D2 で追加する Host-side read-only metadata observer とする。
これは新規 prerequisite であり、現在その coordinator / observer は
存在しないものとして設計する。Cloud 用に local agent/runtime を作らず、
backend の実行・承認・制御権限も増やさない。

lifecycle:

- observer は Host/session の長寿命 lifecycle に属する観測主体とし、
  将来の接続先は `src/supervisor.rs::SessionSupervisor`
  (session 所有・再起動の長寿命 coordinator)とする。
- 起動・登録契機は Host/session lifecycle と、retained task record
  (`TaskStore::list_owned`)からの既存 Cloud task 登録とする。
  dashboard GET、`task_list`、最初の browser access を契機に起動しない。
- Host 再起動後は retained record から owned non-terminal task を
  再発見して観測登録する。session instance の変更・停止、
  task の保持期限 (`TASK_RETENTION_SECONDS` = 24h) 終了、
  observer 停止で登録解除する。観測停止は remote task の停止を意味せず、
  remote task を cancel / resume / recreate しない。

観測所有権と遅延書込みの排除:

- Cloud には runtime generation が存在しないため、task `generation` や
  PID を観測所有権の代用にしない。観測所有権は
  永続 epoch + 期限付き観測 lease を同一の短い排他区間で
  比較更新する方式で確定する(D2 で新規追加する設計であり、
  別の coordination service / execution authority は追加しない)。

所有権 metadata (既存 Cloud task の ownership / retention に従う
record 上の新規 field;既存実装済みではない):

```text
observer_owner_id   observer instance を区別する一意な識別子
                    (task owner、PID、task generation とは別物)
producer_epoch      同じ retained task に対する観測所有権の
                    単調増加カウンタ。release しても保持し、
                    再取得時に増加する
lease_expires_at    観測所有権の期限
                    (summary.expires_at とは別物)
```

所有権と結果の適用対象は task_id だけでなく次の binding に固定する
(既存 Cloud task record が保持する識別情報):
`task_id` + session instance (`owner`) + canonical scope (`scope_cwd`) +
`org_id` + `devin_session_id` + task `generation`。
この識別情報を省略して別 session・別 organization・
再開後の task に結果を適用しない。

観測 lease は 15 秒、更新周期は 5 秒とする。lease の更新は
observer の所有権維持であり、remote state の鮮度確認ではない。
summary TTL 30 秒とは役割を分離する。

原子的に確定する操作 (以下、すべて同一の短い排他区間で
比較 + 更新する):

| 操作 | 許可条件 | 同一排他区間内で行う変更 |
|---|---|---|
| 初回取得・引継ぎ | 対象 binding が有効で、owner 未設定または `now >= lease_expires_at` | `producer_epoch` を checked increment し、新 owner と lease 期限を保存 |
| 更新 (renew) | owner ID・epoch が一致し、`now < lease_expires_at`、対象 binding も一致 | `lease_expires_at` だけを延長 |
| summary 保存 | remote read 開始時の owner ID・epoch・binding が現在値と一致し、`now < lease_expires_at` | 最新 record の summary 部分だけを更新 |
| 明示的 release | owner ID・epoch が現在値と一致 | owner と lease を無効化。`producer_epoch` は保持 |
| 再取得 | 前の lease が失効済み、または正常に release 済み | 同じ observer でも新 epoch を取得。旧 read 結果は再利用しない |

- 期限切れ後の renew は拒否する。新 owner がいなくても古い epoch を
  復活させず、再取得後に新しい remote read を行う。
- 古い cleanup は新 owner を解除しない。release も owner ID・epoch の
  一致条件付きとする。
- epoch は巻き戻し・再利用しない。不正値・overflow・所有権 metadata の
  破損を初期値に戻して続行せず、当該観測を fail-closed にする。
- 削除された task を復活させない。remote response が戻った時点で
  record が削除済み、保持期限終了、session instance 不一致などであれば
  その結果を破棄する。

排他境界と保存順序:

既存 `TaskStore::lock()` は process mutex と、Unix では
`.store.lock` への `flock(LOCK_EX)` を提供し、`save_locked()` は
一時ファイルへの書込みと rename で保存する。
取得判定と永続更新はこの境界で process 間直列化し、
process-local mutex だけでは十分としない。
flock 等の process 間排他を保証できない platform では、
その保証を platform 固有の実装 prerequisite として明記し、
保証済みとは扱わない。

```text
短い排他区間:
  current task / ownership を再読込み
  acquire または renew の条件を検証
  ownership metadata を永続化
  read 用の binding / owner / epoch snapshot を取得
排他解除

remote status read

短い排他区間:
  current task / ownership を再読込み
  snapshot と現在値、lease 有効性を比較
  一致した場合だけ、最新 record の許可された metadata を更新
排他解除
```

network await 中に store lock を保持しない。
network read 前の task record 全体を response 到着後にそのまま
保存してはいけない。並行した既存 task 操作の更新を消さないよう、
最新 record に許可された metadata のみを適用する。

観測で task の保持期限を延長しない:

既存 `TaskStore::update()` は `record.updated_at` を更新し、
retention 判定は `updated_at` を参照する。observer の定期 metadata 更新に
無条件の `update()` を流用すると最終更新時刻と保持期間まで変わるため、
観測 metadata 専用の保存経路で次を保持する:

```text
変更可能:  observation ownership metadata / pending summary metadata
変更不可:  task.updated_at / created_at、task status / revision /
           generation、operation receipts、remote session binding、
           task owner / canonical scope / org_id
```

summary の `observed_at` は remote fetch 成功と状態検証時だけ更新し、
lease renew や observer の生存確認で更新しない。

検証例 (時間単位は秒、失効条件は `now >= lease_expires_at`):

```text
初期: epoch = 40, owner = none
t=0   A が acquire → owner=A, epoch=41, lease_expires_at=15
t=1   B が acquire → 拒否。A/41 を維持
t=16  A は未更新で期限切れ。B が acquire
      → owner=B, epoch=42, lease_expires_at=31
t=17  A/41 の古い remote result が到着
      → 保存拒否。B/42 の metadata は変わらない
t=18  A/41 の遅延 renew / release → 拒否。B/42 を維持
```

許可する取得と保存:

- 許可する取得は、既に binding された remote session の
  status read (`status` / `status_detail`) のみとする。
  full `task_get`、reconciliation、evidence 生成、message 取得を
  observer の取得経路として流用しない。
- observer が変更できるのは新しい summary metadata と
  観測所有権 metadata のみ。task status、operation receipt、
  backend generation、remote session binding を観測の都合で変更しない。
- credential は既存 `resolve_config()` (src/devin_cloud.rs の
  module-level 関数) の解決経路を使い、API key は呼び出し中だけ
  メモリに保持し、新規 metadata、log、serialize 出力に
  認証値を保存しない。

state / freshness:

```text
status_detail = waiting_for_approval を確認
  → pending (type = approval; count は捏造しない)
status read 成功 + pending signal なしを検証
  → none
未解釈 / 欠落した status 値
  → unknown
fetch 失敗 / timeout / rate limit / credential failure
  → unavailable
```

- `observed_at` は remote fetch 成功 + 状態検証時のみ更新し、
  observer process の生存だけで remote 状態の鮮度を延長しない。
- interval / timeout / concurrency / TTL / serialized bound は
  共通節の値を使う。失敗時の再試行は同一 interval 内で最大 3 回までとし、
  observer の lease / epoch と summary TTL は役割を分離する。

安全性:

- prompt、質問本文、選択肢本文、stdout/stderr、argv、raw evidence を
  Worker / browser へ取得してから隠す方式にしない。
  safe metadata は Host 側で生成し、bounded な読み取り契約にする。
- 既存の `*_task_get` を全 task に無制限 fan-out しない。
- `task_get` の暗黙の reconciliation / 副作用を
  read-only list の延長として無検討に持ち込まない。

`orchestration.rs` の `task_list` は retained state の read であり、
必ずしも backend の現在状態を再照会した結果ではない。
host に接続できた時刻を、そのまま task state の観測時刻にしない。
Host 由来であることと backend state が最新確認済みであることを分離する。

表示:

- backend
- task_id short display
- state
- updated_at
- pending interaction summary(上記 state / count / types / freshness)
- reconciliation / unavailable status
- model/effort only when already returned by the bounded task projection

backend store failure を zero tasks と表示しない。

### 8.5 Context

existing `context_resolve` / `context_status` semantics を presentation する。

表示:

- current task/execution/workspace state
- unresolved items
- recent bounded rollups
- provenance refs
- journal/cloud revision
- compaction/degradation
- memory worker state

raw observation body は返さない。

host live resolver が unavailable で Fabric-side offline context resolver がまだ成立していない場合は
`authority: unavailable` として表示し、replicated data から current state を推測生成しない。

### 8.6 Timeline

timeline は local `activity` socket を remote bridge しない。

Fabric D1 に replicate 済みの allow-listed observation envelope から、
dashboard 用の bounded metadata projection を作る。

初期表示は最大 100 件、API 上限は 256 件程度の固定 bound とする。
cursor/revision で incremental fetch できるようにする。

timeline に含めないもの:

- prompt body
- stdout/stderr
- command argv
- environment values
- credential/token
- raw evidence body
- hidden reasoning / transcript

## 9. Refresh / degraded behavior

v1 は WebSocket / SSE を必須にしない。

- foreground: 5 秒 polling
- background tab: 30 秒 polling
- manual refresh
- API が revision/cursor を返す場合は incremental fetch
- concurrent duplicate fetch を抑止
- stale response が新しい revision を上書きしない

host が offline になった場合:

```text
LIVE -> OFFLINE
last live state -> visually stale
replica -> separate "last synchronized" section
```

とし、最後に見えた `running` を現在も running と表示し続けない。

切断 / lease expiry で registry から消えた host も、
§8.2 の inventory contract により page reload 後も
configured membership に基づく entry として再構成される。
availability は再評価結果に従う(live route なし → `offline`、
registry 読み取り失敗 → `unknown`)。
接続 / 同期履歴は証拠が存在する場合のみ表示し、
証拠がなければ `unknown`、履歴取得元の失敗は `unavailable` とする。
offline host の retained task を current running と表示しない。

## 10. UI v1

1画面で master/detail にする。

### Header

- Temote Fabric
- deployment/version
- contract fingerprint short form
- refresh status
- degraded/error indicator

### Left: Hosts / Sessions

- hosts
- selected host sessions
- online/offline/unknown/unavailable badges

### Center: Session / Tasks

- session summary
- workspace identity
- tasks by backend
- pending interaction marker

### Right / lower pane: Context / Timeline

- unresolved items
- freshness
- recent task/context rollup
- sanitized timeline

mobile-perfect layout は v1 acceptance にしないが、狭い browser でも information が失われない responsive layout にする。

## 11. Security / privacy requirements

- all dashboard routes authenticated
- no third-party scripts, fonts, analytics, error reporting SDK
- no secrets in HTML boot payload
- no Access JWT / cookie / service token in logs
- dashboard API uses `Cache-Control: no-store`
- API does not enable permissive CORS; same-origin browser use only
- CSP at minimum:
  - `default-src 'self'`
  - `script-src 'self'`
  - `style-src 'self'`
  - `connect-src 'self'`
  - `img-src 'self' data:`
  - `frame-ancestors 'none'`
  - `base-uri 'none'`
  - `form-action 'none'`
- `X-Content-Type-Options: nosniff`
- sensitive identifiers are bounded before rendering/logging
- no localStorage persistence of task/session data in v1
- dashboard does not weaken Fabric / Host authorization boundaries
- dashboard responses (success / auth failure / 404 / 405 / error) do not pass through the existing permissive `withCors()` wrapper or the global OPTIONS handler

## 12. Failure semantics

Dashboard は partial failure を first-class にする。

例:

- Fabric Worker healthy / one host unavailable
- host online / one backend task store unavailable
- live session read succeeds / observation replica has gap
- D1 unavailable / live host read succeeds
- host registry liveness unavailable / configured membership と replica metadata は読める
  (inventory は membership entry として表示を継続し、
  availability は `unknown`、消えない)
- context resolver unavailable / session list succeeds

一部失敗を page 全体の empty state に変換しない。

API response は component 単位に:

```text
status = confirmed | unavailable | stale
error_code?
authority?
freshness?
```

を持てる構造にする。

error detail に credential / upstream raw response body を流さない。

## 13. Implementation packets

### D0 — route/auth/assets contract

- `/dash` routing
- dashboard Access-only auth mode(§6 の prerequisite;
  `authorizeClient()` の CLIENT_TOKEN fallback を dashboard に適用しない)
- `access.js` の Access JWT 検証ロジックの再利用
- assets binding + `run_worker_first`
- dashboard-scoped response path(no-store / same-origin / §11 security headers;
  global OPTIONS / permissive `withCors()` wrapper を通さない)
- no behavior change to existing endpoints
- tests only with static shell

D0 の将来検証ケース:

- JWT なし → 拒否
- 有効な `CLIENT_TOKEN` のみ → 拒否
- 有効な `CLIENT_TOKEN` + 不正/期限切れ JWT → 拒否
- 正しく検証された許可対象の Access JWT → 許可
- 不正な issuer / audience / signature / email → 拒否
- `/dash`、`/dash/`、JS/CSS、API、unknown `/dash/*` path でも
  認証前に dashboard content を返さない
- `/mcp` の既存認証動作(`CLIENT_TOKEN` 受理)は変わらない

### D1 — host/session dashboard

- bootstrap
- host inventory(§8.2: configured membership + owner-scoped replica metadata
  の情報源属性付き合成)
- `readOnlineHosts` liveness overlay → availability
  (`online` / `offline` / `unknown` = live route の有無のみ)
- connection / sync history 表示契約(証拠なし → `unknown`、
  履歴取得元失敗 → `unavailable`、
  `last_synced_at` は同期時刻であり切断時刻ではない)
- session list/info
- master/detail UI
- replica freshness metadata(`last_synced_at` / gap / degraded)
- membership 認可を host 配下の直接 API 参照へも一貫して適用

D1 の将来検証ケース:

- ケース A(未接続 host)とケース B(一度も同期せず切断し
  registry entry も prune 済みの host)が同じ取得結果になるとき、
  どちらにも未確認の接続履歴を付与しない
- observation を一度も同期せず切断した host を
  「以前接続していた」と断定しない
- 接続 → 切断 / lease expiry → registry prune → page reload 後も、
  inventory に残る対象 host を識別できる
- D1 単独障害: live route 確認済み host は `online`、
  replica metadata component のみ `unavailable`
- registry 単独障害: availability は `unknown`、
  inventory は membership entry として表示を継続する
- 設定から削除 / 認可対象外となった host の直接参照を拒否し、
  stale D1 row から復元表示しない
- 破損した membership 設定や不完全な registry 結果を
  正常な空集合として扱わない
- 設定から削除された host / 別 owner / 同名 session を混同しない
- offline host の retained task を current running と表示しない

### D2 — task projection

- `task_list` projection の利用 (retained state の read のみ)
- Host prerequisite A: `runtime_owner` producer (Codex / OpenCode /
  Devin ACP) による pending metadata 生成
  (§8.4: backend 別 safe source、interval 5s / timeout 2s /
  concurrency 4 / TTL 30s / count ≤ 64 / types ≤ 4 / summary ≤ 4KiB)
- Host prerequisite B: task metadata への bounded summary 保存と
  `task_list` の additive projection
- Host prerequisite C (Devin Cloud): Host-side read-only metadata observer
  と観測所有権 (owner identity / `producer_epoch`) / lifecycle 登録契約
  (§8.4「Devin Cloud observer」)
- per-backend unavailable semantics
- pending interaction display
  (`none | pending | unknown | unsupported | unavailable`)
- `summary_revision` による task status / revision 非依存の更新検出
- bounded refresh
- 上記 Host prerequisite が揃うまで、
  pending interaction 表示機能は未完了とする

D2 の将来検証ケース:

- 通常の `task_get` を一度も呼ばなくても、対応 backend の
  pending なし → あり → 解消を summary で観測できる
- task status / task revision が変わらなくても、
  pending summary の変更が UI に反映される
- observer 停止・期限切れ・一部 endpoint 失敗時、
  古い `none` を現在の「対話待ちなし」として表示しない
- producer epoch 変更後、旧 owner の遅延結果を採用しない
- `task_list` / dashboard GET だけでは
  spawn・resume・reconciliation・answer が発生しない
- Cloud task 作成後、元の tool call が戻った状態でも、
  通常の `task_get` を呼ばず Host-side observer が summary を更新できる
- dashboard を一度も開いていなくても、許可された観測 lifecycle が
  browser access と無関係に成立する
- Cloud observer が二つ競合しても、current owner だけが書き込み、
  所有権交代前の遅延結果を拒否する
- remote read 中に task binding / session instance が変わっても、
  古い対象への結果を新しい対象の summary に適用しない
- Host / observer 停止時、summary は期限切れとして扱うが
  remote task は停止しない
- Cloud observer の timeout / rate limit / credential failure は
  `none` に変換せず `observed_at` も更新しない
- 同時 acquire が競合しても owner と epoch が一意に決まる
- 期限切れ後に旧 owner を復活させない (遅延 renew / release / result を拒否)
- remote read 中に binding が変わった応答、task 削除・保持期限終了・
  session instance 不一致の応答を採用しない
- 観測 metadata の更新が `task.updated_at` と保持期限を変えない
- 質問本文・選択肢本文・permission detail・raw result は
  summary 保存先、Worker response、browser、ログへ流れない
- field を返さない旧 Host は `unsupported`
- 一部 backend 取得不能
- summary 上限超過の truncation
- raw task body を含む upstream fixture でも
  dashboard 向け response には allow-listed metadata のみ出る
- retained / stale state を最新実行状態と誤表示しない

### D3 — context/timeline

- context resolve/status projection
- replicated observation metadata query
- provenance/freshness/gap UI
- no raw bodies

### D4 — deployed Zero Trust acceptance

real deployed Fabric hostname で:

- unauthenticated browser blocked by Access
- authorized browser loads `/dash/`
- dashboard API rejects direct/bypassed unauthenticated request at Worker auth
- `/dash` rejects a `CLIENT_TOKEN`-only request at Worker auth; `/mcp` keeps its existing CLIENT_TOKEN compatibility
- host/session/task projection renders
- offline host is not presented as live
- `/mcp` contract fingerprint unchanged
- observation sync still works
- `/healthz` semantics unchanged

各 packet は独立 review 可能なサイズにする。

## 14. Acceptance criteria

- [ ] Dashboard は existing Fabric Worker と同じ deployment に含まれる。
- [x] canonical entry is `/dash/`; `/dash` は authenticated redirect。
- [ ] Cloudflare Access を通らない public dashboard route が存在しない。
- [x] Worker 側で検証済み Access JWT(signature / issuer / audience / expiry / subject / `ACCESS_ALLOWED_EMAILS`)を必須とする。
- [x] `CLIENT_TOKEN` / `HOST_TOKEN` / federated host token は dashboard 認証の代替にならない。
- [x] Dashboard v1 は read-only。
- [x] Existing `/mcp`, `/healthz`, host API, observation ingest の behavior を壊さない。
- [x] host/session/task live state と Fabric replica の authority が UI/API で区別される。
- [x] offline/unavailable を empty/success/current に正規化しない。
- [x] host inventory が server-side configured membership と owner-scoped replica metadata で構成され、`readOnlineHosts` は liveness overlay に限定される。
- [x] offline host が registry prune / page reload 後も識別できる。
- [x] availability (`online` / `offline` / `unknown`) は live route の有無のみを表し、判別不能な接続履歴や切断理由を断定しない (ケース A / B に未確認の履歴を付与しない)。
- [x] host の接続 / 同期履歴は証拠が存在する場合のみ表示され、証拠なしは `unknown`、履歴取得元の失敗は `unavailable`。`configured_only` は情報源属性であり「未接続」の意味を持たない。
- [x] configured membership 認可が host 一覧と host 配下の直接参照に一貫して適用され、stale D1 row からの復元表示をしない。
- [x] unknown / offline / unavailable と evidence 属性を混同しない。
- [x] pending interaction は `runtime_owner` producer または `host_remote_observer` が生成し task metadata に保存された bounded summary を `task_list` が読み取る経路で表示され、欠落 field を「なし」と表示しない。
- [x] pending summary は `summary_revision` / `observed_at` / `producer_kind` / `producer_epoch` / `expires_at` を持ち、期限切れ・一部 endpoint 失敗・observer 停止を「対話待ちなし」と表示しない。
- [x] task status / revision 非依存の summary 変更が UI に反映される。
- [x] `task_list` / dashboard GET だけでは task/runtime の spawn・resume・reconciliation・answer が発生しない。
- [x] Devin Cloud の summary は D2 の Host-side read-only observer が更新し、dashboard GET / task_list / browser access を契機に起動・取得しない。
- [x] 観測所有権 (`producer_kind` / `producer_epoch`) と実行所有権を混同せず、旧 owner の遅延結果を拒否する。
- [x] Cloud observer は binding 済み remote session の status read のみを行い、task status / operation receipt / backend generation / remote session binding を観測の都合で変更しない。
- [x] 観測所有権は `observer_owner_id` / 永続 `producer_epoch` / 15 秒 lease を同一排他区間で比較更新し、acquire・renew・expire・takeover・publish・release の許可条件と保存内容が一意に決まる。
- [x] 観測 metadata 更新は task `updated_at` / `created_at` / status / revision / generation / receipts / binding / owner / scope / org_id を変更しない。
- [x] Host / observer 停止時、summary は期限切れとして扱うが remote task は停止しない。
- [x] process restart 後、保存済み summary を現在の owner・接続状態・鮮度の確認なしに heartbeat だけで fresh に戻さない。
- [x] task list の backend-level unavailable が表示可能。
- [x] context freshness / gap / degradation が表示可能。
- [x] timeline は sanitized replicated observation の bounded projection のみ。
- [x] prompt/stdout/stderr/argv/environment/credential/raw evidence を dashboard data に追加しない。
- [x] dashboard API は no-store + same-origin + strict security headers。
- [x] dashboard assets は third-party runtime/CDN に依存しない。
- [x] `npm test --prefix gateway` PASS。
- [x] `wrangler deploy --dry-run` PASS。
- [ ] deployed Zero Trust E2E を記録する。
- [x] `git diff --check` PASS。

## 15. Non-goals / follow-up

この issue では実装しない:

- session stop/restart
- task steer/resume/interrupt
- OpenCode permission/question answer
- Devin approval bridge
- permission mutation
- repository/VCS mutations
- PR delivery action
- terminal embedding
- raw activity stream remote exposure
- arbitrary log viewer
- token/cost analytics
- multi-user RBAC beyond existing Access policy
- dashboard-specific auth database
- public dashboard
- separate Cloudflare Pages application

write controls を追加する場合は、read dashboard の完成後に別 issue とし、
CSRF / operation id / approval / idempotency / stale-state revalidation を個別に設計する。

## 16. Cloudflare platform references

Design baseline checked 2026-09-29:

- Cloudflare Access for Workers:
  https://developers.cloudflare.com/workers/configuration/cloudflare-access/
- Workers Static Assets / SPA and worker-first routing:
  https://developers.cloudflare.com/workers/static-assets/routing/single-page-application/
- Static Assets worker-script behavior / Access context limitation:
  https://developers.cloudflare.com/workers/static-assets/routing/worker-script/

Platform-specific behavior must be rechecked when implementation starts; repo security/authority invariants remain the source of truth.

## 17. Implementation tracking

D0–D4 implementation and gates are tracked in [the acceptance matrix](../../docs/evaluations/fabric-dashboard-20260929.md). The design-stage statement in §6 does not restrict the authorized runtime implementation. This issue remains open until all acceptance gates are evidenced. Deployed acceptance and independent PR review are separate from local tests.

D0–D3 code and local regression gates pass. Checked §14 items denote local implementation/test evidence, not deployed or independent approval. Same-deployment identity, Access edge acceptance, and deployed Zero Trust E2E remain unchecked pending approved candidate Worker/Host rollout and browser login. The acceptance matrix contains layer-specific results and the operator rerun.
