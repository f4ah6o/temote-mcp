# Fabric Dashboard: Zero Trust 配下の `/dash` Web dashboard

Status: open / design ready  
Repository: `f4ah6o/temote-mcp`  
Priority: P1 operator visibility  
Created: 2026-09-29 (Asia/Tokyo)  
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

host ごとの表示状態を次で区別する:

```text
online             現在接続を確認できた host
                   (registry entry + status probe 成功)
offline            既知だが lease expiry / disconnect を確認した host
unknown            registry / probe 失敗で現在状態を判断できない host
configured_only    configured だが接続履歴も observation もない host
                   (offline の一種として扱い、history なしと区別して表示)
```

- unknown を offline と断定しない。offline を一覧から消して済ませない。
- observation がないことを「host が存在しない」と同一視しない。
- `last seen` は復元できる場合のみ表示する。registry は lease (`expires_at`)
  しか保持しないため、prune 後の last seen は `observation_sources.last_synced_at`
  を "last synchronized" として表示し、復元できない場合は `unknown` とする。
  時刻を捏造しない。
- 認可対象から外れた host は inventory に含めない。
- D1 が失敗しても live read が成功する host は `online` のまま、
  replica metadata component を `unavailable` として表示する (§12)。

表示:

- host_id
- availability (上記 state)
- last synchronized / lease freshness if available
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
Host-side bounded `pending_interaction` summary の合成とする。

現行の `compact_task_list_view` は各 task を
backend / task_id / status / revision / last_updated_at に縮約するため、
existing projection だけでは pending interaction の有無を取得する経路がない。
欠落 field を `count=0` / `pending=false` に変換しない。

D2 prerequisite として、既存 `task_list` に additive な
safe summary 拡張を Host 側に追加する(完成した API ではなく前提契約):

- 各 task item に allow-listed metadata のみを持つ
  bounded `pending_interaction` summary を付加する。
- 追加 MCP tool や任意 tool-name を受ける dashboard proxy は作らない。
- 新しい execution authority / task control 経路を作らない。
- task owner / session / workspace の既存検証を迂回しない。
- 既存 field と入力契約を維持し、
  public MCP contract fingerprint を不必要に変更しない。

summary が表現する内容:

```text
state        = none | pending | unknown | unsupported | unavailable
count?       bounded integer
types?       allow-listed interaction type のみ (bounded)
revision / observed_at   freshness
truncated?   上限に達したことを示す marker
```

件数・配列・文字列は固定上限を持ち、超過は `truncated` で表現する。

- `none` は取得元が正当に pending なしを確認できた場合のみとする。
- field を返さない旧 Host は `unsupported` とし、
  「対話待ちなし」と表示しない。
- backend / Host / store の取得失敗は `unavailable` とし、
  該当 backend の他 task には影響させない。
- 全 backend を恒久的に `unknown` にするだけで
  pending interaction 機能が完成した扱いにはしない。

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
`offline` として再構成できるようにする。
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
  (inventory は `unknown` state のまま表示し、消えない)
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
- host inventory(§8.2: configured membership + owner-scoped replica metadata)
  と `readOnlineHosts` liveness overlay の合成
- session list/info
- master/detail UI
- online / offline / unknown / configured-only semantics
- replica freshness metadata(`last_synced_at` / gap / degraded)

D1 の将来検証ケース:

- 接続 → 切断 / lease expiry → registry prune → page reload 後も、
  inventory に残る対象 host を offline として識別できる
- observation 未同期の既知 host を混同しない
- registry failure と D1 failure が独立した partial failure になる
- 設定から削除された host / 別 owner / 同名 session を混同しない
- offline host の retained task を current running と表示しない

### D2 — task projection

- `task_list` projection の利用
- Host prerequisite: per-task bounded `pending_interaction` summary
  (§8.4、additive で safe metadata のみ)
- per-backend unavailable semantics
- pending interaction display(`none | pending | unknown | unsupported | unavailable`)
- bounded refresh
- summary 拡張の Host prerequisite が揃うまで、
  pending interaction 表示機能は未完了とする

D2 の将来検証ケース:

- pending あり
- 確認済み pending なし
- field を返さない旧 Host
- 一部 backend 取得不能
- pending summary 未対応 backend
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
- [ ] canonical entry is `/dash/`; `/dash` は authenticated redirect。
- [ ] Cloudflare Access を通らない public dashboard route が存在しない。
- [ ] Worker 側で検証済み Access JWT(signature / issuer / audience / expiry / subject / `ACCESS_ALLOWED_EMAILS`)を必須とする。
- [ ] `CLIENT_TOKEN` / `HOST_TOKEN` / federated host token は dashboard 認証の代替にならない。
- [ ] Dashboard v1 は read-only。
- [ ] Existing `/mcp`, `/healthz`, host API, observation ingest の behavior を壊さない。
- [ ] host/session/task live state と Fabric replica の authority が UI/API で区別される。
- [ ] offline/unavailable を empty/success/current に正規化しない。
- [ ] host inventory が server-side configured membership と owner-scoped replica metadata で構成され、`readOnlineHosts` は liveness overlay に限定される。
- [ ] offline host が registry prune / page reload 後も識別できる。
- [ ] unknown / offline / configured-only / unavailable を混同しない。
- [ ] pending interaction は Host 側 bounded summary 経由で表示され、欠落 field を「なし」と表示しない。
- [ ] task list の backend-level unavailable が表示可能。
- [ ] context freshness / gap / degradation が表示可能。
- [ ] timeline は sanitized replicated observation の bounded projection のみ。
- [ ] prompt/stdout/stderr/argv/environment/credential/raw evidence を dashboard data に追加しない。
- [ ] dashboard API は no-store + same-origin + strict security headers。
- [ ] dashboard assets は third-party runtime/CDN に依存しない。
- [ ] `npm test --prefix gateway` PASS。
- [ ] `wrangler deploy --dry-run` PASS。
- [ ] deployed Zero Trust E2E を記録する。
- [ ] `git diff --check` PASS。

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
