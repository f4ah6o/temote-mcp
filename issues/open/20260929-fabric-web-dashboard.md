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
- federated host 一覧と online / offline / unavailable
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

現行 Fabric は `authorizeClient(request, env)` で Access JWT を検証している。
Dashboard もこれを再利用する。

```text
Access edge auth
        +
Worker authorizeClient()
```

の二段を維持する。

Cloudflare Workers の `ctx.access` への移行はこの issue の成立条件にしない。
Static Assets routing との identity propagation 制約があるため、現行 request/JWT validator を捨てる変更は別 packet とする。

`/dash` asset request も Worker-first で処理して authorization 後に assets binding へ渡し、
direct-origin / route misconfiguration でも dashboard asset を無認証配信しない。

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

Static Assets は binding 経由で serving し、`/dash` と `/dash/*` は Worker を先に通す。
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

existing `readOnlineHosts` / registry projection を再利用する。

表示:

- host_id
- availability
- last seen / lease freshness if available
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

existing `task_list` projection を source にする。

表示:

- backend
- task_id short display
- state
- updated_at
- pending interaction count / type when present
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
- online/offline/unavailable badges

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

## 12. Failure semantics

Dashboard は partial failure を first-class にする。

例:

- Fabric Worker healthy / one host unavailable
- host online / one backend task store unavailable
- live session read succeeds / observation replica has gap
- D1 unavailable / live host read succeeds
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
- Access/JWT reuse
- assets binding
- security headers
- no behavior change to existing endpoints
- tests only with static shell

### D1 — host/session dashboard

- bootstrap
- host list
- session list/info
- master/detail UI
- live/offline/unavailable semantics

### D2 — task projection

- `task_list` reuse
- per-backend unavailable semantics
- pending interaction display
- bounded refresh

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
- [ ] Worker 側でも existing Access JWT verification を通す。
- [ ] Dashboard v1 は read-only。
- [ ] Existing `/mcp`, `/healthz`, host API, observation ingest の behavior を壊さない。
- [ ] host/session/task live state と Fabric replica の authority が UI/API で区別される。
- [ ] offline/unavailable を empty/success/current に正規化しない。
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
