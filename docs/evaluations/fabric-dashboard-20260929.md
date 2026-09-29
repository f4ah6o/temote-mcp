# Fabric dashboard acceptance and verification, 2026-09-29

Issue: [Fabric dashboard](../../issues/open/20260929-fabric-web-dashboard.md).

This record separates implementation, local checks, deployed acceptance, CI, and independent review. No merge or release is authorized.

## Starting identity

- Repository: `f4ah6o/temote-mcp`, `/home/hirohito-fujita/src/temote-mcp`.
- Clean `main`, HEAD/upstream `ae24006798b7a222702e40ffc2b446ce5b9acd0d` (`origin/main`); default branch `main`.
- Remote: `https://github.com/f4ah6o/temote-mcp.git`; one worktree; no dirty/untracked files.
- Dedicated branch: `feat/fabric-web-dashboard`; no unrelated unmerged PR incorporated.
- Open PRs at start: #81 memory plane, #74 issue triage, #63 Devin tier, #61 friction, #55 task state separation. Existing core is used; these are not implicitly dependencies.
- Existing repository-scoped `gh git` binding: `f4ah6o`, managed; doctor passed. Global active account differs; direct GitHub commands use the existing tokenless repository profile. No binding/account/remote change.
- Available model inventory: `gpt-6-sol` medium and `gpt-6-luna` max. Implementation agents use Luna max.
- Baseline build completed before Rust edits; binaries/helper preserved under ignored `dogfood/runs/fabric-dashboard-baseline/`.
- Baseline binary SHA-256: `3693a9bb142df5f198e45d7b55eab48cea1ffa28d8f67fbd343f68636f4b6221`; helper `e457aaff08947e78f8da3fdb44d7e27eaa67f21fe0d9ef35e14eae13b272f95a`.
- Temote app-server 0.147.0 advertises `gpt-5.6-luna / max`, unlike the desktop GPT-6 inventory. Initial requested-model live run `b5bd802e-7c0c-4e38-84bc-8ef7e03c7f83` failed terminally; assertions for bounded result/no duplicate are NOT RUN. No passing comparison is claimed for that run.
- Baseline fixture delegation lifecycle passed (run `8e05dffa-632c-439a-ae7e-24b8fcacafb5`, five calls). This is not live-provider or dashboard acceptance evidence.

## Platform references

Issue §16 official Access/Static Assets/Worker-script docs rechecked 2026-09-29. Worker-first routing is required before assets authentication; assets binding honors HTML handling. Fixed Wrangler is `4.142.0`; local schema includes `run_worker_first` and `html_handling`. No SPA fallback or external runtime.

## Acceptance matrix

Results start as NOT RUN and must be updated from direct evidence. Scope and requirements remain those of the issue.

| ID / packet | Requirement | Implementation | Verification | Result | Evidence | Blocker |
| --- | --- | --- | --- | --- | --- | --- |
| AC01 / D0 | Dashboard は existing Fabric Worker と同じ deployment に含まれる。 | `gateway/wrangler.toml`; `src/dashboard/index.js` | Pinned Wrangler deploy dry-run; real workerd assets | LOCAL PASS | D0 commit `9c221c6`; local asset tests `d3d4768` | D4 deployed identity NOT RUN |
| AC02 / D0 | canonical entry is `/dash/`; `/dash` は authenticated redirect。 | `handleDashboard`; exact canonical asset allow-list | Signed JWT redirect; unauthenticated/alias rejection | PASS | 80 real workerd tests/subtests; security tests | — |
| AC03 / D4 | Cloudflare Access を通らない public dashboard route が存在しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC04 / D0 | Worker 側で検証済み Access JWT(signature / issuer / audience / expiry / subject / `ACCESS_ALLOWED_EMAILS`)を必須とする。 | `authorizeDashboard`; shared JWT verifier | RSA fixture signature/issuer/aud/exp/nbf/sub/email failures | PASS | `dashboard-security.test.mjs`; real signed JWT asset test | Deployed JWT gate remains D4 |
| AC05 / D0 | `CLIENT_TOKEN` / `HOST_TOKEN` / federated host token は dashboard 認証の代替にならない。 | Explicit Access-only mode; no shared env mutation | Client/host/federated tokens denied on all tested routes | PASS | Security + 80 real workerd tests/subtests | — |
| AC06 / D0 | Dashboard v1 は read-only。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC07 / D0 | Existing `/mcp`, `/healthz`, host API, observation ingest の behavior を壊さない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC08 / D1 | host/session/task live state と Fabric replica の authority が UI/API で区別される。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC09 / D1 | offline/unavailable を empty/success/current に正規化しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC10 / D1 | host inventory が server-side configured membership と owner-scoped replica metadata で構成され、`readOnlineHosts` は liveness overlay に限定される。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC11 / D1 | offline host が registry prune / page reload 後も識別できる。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC12 / D1 | availability (`online` / `offline` / `unknown`) は live route の有無のみを表し、判別不能な接続履歴や切断理由を断定しない (ケース A / B に未確認の履歴を付与しない)。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC13 / D1 | host の接続 / 同期履歴は証拠が存在する場合のみ表示され、証拠なしは `unknown`、履歴取得元の失敗は `unavailable`。`configured_only` は情報源属性であり「未接続」の意味を持たない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC14 / D1 | configured membership 認可が host 一覧と host 配下の直接参照に一貫して適用され、stale D1 row からの復元表示をしない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC15 / D1 | unknown / offline / unavailable と evidence 属性を混同しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC16 / D2 | pending interaction は `runtime_owner` producer または `host_remote_observer` が生成し task metadata に保存された bounded summary を `task_list` が読み取る経路で表示され、欠落 field を「なし」と表示しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC17 / D2 | pending summary は `summary_revision` / `observed_at` / `producer_kind` / `producer_epoch` / `expires_at` を持ち、期限切れ・一部 endpoint 失敗・observer 停止を「対話待ちなし」と表示しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC18 / D2 | task status / revision 非依存の summary 変更が UI に反映される。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC19 / D2 | `task_list` / dashboard GET だけでは task/runtime の spawn・resume・reconciliation・answer が発生しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC20 / D2 | Devin Cloud の summary は D2 の Host-side read-only observer が更新し、dashboard GET / task_list / browser access を契機に起動・取得しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC21 / D2 | 観測所有権 (`producer_kind` / `producer_epoch`) と実行所有権を混同せず、旧 owner の遅延結果を拒否する。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC22 / D2 | Cloud observer は binding 済み remote session の status read のみを行い、task status / operation receipt / backend generation / remote session binding を観測の都合で変更しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC23 / D2 | 観測所有権は `observer_owner_id` / 永続 `producer_epoch` / 15 秒 lease を同一排他区間で比較更新し、acquire・renew・expire・takeover・publish・release の許可条件と保存内容が一意に決まる。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC24 / D2 | 観測 metadata 更新は task `updated_at` / `created_at` / status / revision / generation / receipts / binding / owner / scope / org_id を変更しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC25 / D2 | Host / observer 停止時、summary は期限切れとして扱うが remote task は停止しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC26 / D2 | process restart 後、保存済み summary を現在の owner・接続状態・鮮度の確認なしに heartbeat だけで fresh に戻さない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC27 / D1 | task list の backend-level unavailable が表示可能。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC28 / D3 | context freshness / gap / degradation が表示可能。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC29 / D3 | timeline は sanitized replicated observation の bounded projection のみ。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC30 / D1 | prompt/stdout/stderr/argv/environment/credential/raw evidence を dashboard data に追加しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC31 / D0 | dashboard API は no-store + same-origin + strict security headers。 | `dashboardResponse` on success/error/redirect/unknown/method paths | No CORS; no-store/CSP/nosniff/same-origin headers | PASS | Handler + real workerd response assertions | — |
| AC32 / D0 | dashboard assets は third-party runtime/CDN に依存しない。 | Plain local ES module/stylesheet assets | Review imports/resources and asset response contents | PASS | `gateway/assets/dash/`; real asset routing | — |
| AC33 / D1 | `npm test --prefix gateway` PASS。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC34 / D4 | `wrangler deploy --dry-run` PASS。 | Same Worker assets binding; worker-first; no fallback | `npm run deploy:dry-run --prefix gateway` | PASS (D0 checkpoint) | Pinned Wrangler `4.142.0`; `/tmp/fabric-d0-dryrun2.log` | Final integrated dry-run pending |
| AC35 / D4 | deployed Zero Trust E2E を記録する。 | Candidate deploy/Host update rerun below | Deployed Zero Trust edge + Worker + Host acceptance | BLOCKED / NOT RUN | Pre-candidate edge 401 is explicitly separate | No approved candidate deploy/Host update; candidate browser session not established |
| AC36 / D1 | `git diff --check` PASS。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |

## Gate log

| Gate | Result | Evidence / limitation |
| --- | --- | --- |
| Baseline build | PASS | `cargo build --bins --locked`; retained identities above |
| Baseline live dogfood | PASS | Run `d6912c1e-a46f-4831-98f0-c0b89281d6c8`, `codex / gpt-5.6-luna / max`, all assertions pass, 34 calls, 31 polls; baseline binary identity above. |
| Baseline fixture dogfood | PASS | Immutable owner-local run above; not live |
| Local Rust / gateway baseline | PASS | `cargo test`: 151 library + 838 binary + integration suites pass; 9 ignored gates not run. Gateway 102/102 pass. |
| Candidate local tests | NOT RUN | Pending implementation |
| Actual Static Assets runtime | PASS (local) | 80 tests/subtests; real Wrangler/workerd and checked-in assets binding. Synthetic issuer JWKS is the sole network fixture; production JWT verification remains active. No D4 claim. |
| Deployed D4 | BLOCKED | Candidate deployment and Host update are not approved in this session; authorized browser candidate session not established. Existing target evidence alone is not candidate evidence. |
| Final-head CI | NOT RUN | PR / final push pending |
| Independent review | NOT RUN | PR handoff required; local review is separate |

## D4 operator rerun

Existing operational reference: `docs/evaluations/fabric-dogfood-20260928.md` and `docs/gateway.md`. Confirm same deployment target `temote.obr-grp.com`, Access policy and current bindings read-only before approved deployment. Use candidate assets configuration with the existing remote configuration; checked-in placeholders cannot be deployed. Obtain approval for candidate Worker deployment and necessary Host binary update/restart without stopping active tasks. Authorized human browser must log in through Access. Test edge unauthenticated rejection separately from direct Worker JWT rejection, then all dashboard components, offline semantics, MCP/health fingerprint and observation sync. Record exact Worker version, Host binary identity, candidate HEAD and each layer. No bypass/public origin or account change.

## Regression baseline

A source-isolated Worker handler probe at starting HEAD returned 404 with `Access-Control-Allow-Origin: *` and no cache policy for `/dash`, `/dash/`, `/dash/app.js`, and `/dash/api/v1/bootstrap` with a fixture client token. Dashboard regressions require authenticated 401 and private security headers instead. Existing public `https://temote.obr-grp.com/dash/` returned unauthenticated HTTP 401; this is pre-candidate edge evidence only and does not validate the new Worker or D4.
