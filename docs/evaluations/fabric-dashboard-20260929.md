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
| AC03 / D4 | Cloudflare Access を通らない public dashboard route が存在しない。 | Same Worker; workers.dev disabled; Worker-first Access-only routing | Local route/assets checks; approved deployed Access acceptance | BLOCKED / NOT RUN | Real local assets pass; pre-candidate edge 401 is separate | No approved candidate deployment / authorized browser |
| AC04 / D0 | Worker 側で検証済み Access JWT(signature / issuer / audience / expiry / subject / `ACCESS_ALLOWED_EMAILS`)を必須とする。 | `authorizeDashboard`; shared JWT verifier | RSA fixture signature/issuer/aud/exp/nbf/sub/email failures | PASS | `dashboard-security.test.mjs`; real signed JWT asset test | Deployed JWT gate remains D4 |
| AC05 / D0 | `CLIENT_TOKEN` / `HOST_TOKEN` / federated host token は dashboard 認証の代替にならない。 | Explicit Access-only mode; no shared env mutation | Client/host/federated tokens denied on all tested routes | PASS | Security + 80 real workerd tests/subtests | — |
| AC06 / D0 | Dashboard v1 は read-only。 | Fixed read-only host tool allow-list; GET-only private router | Forbidden dispatch fixtures; method/OPTIONS denial | LOCAL PASS | Gateway/API + real workerd tests | D4 live operation proof pending |
| AC07 / D0 | Existing `/mcp`, `/healthz`, host API, observation ingest の behavior を壊さない。 | Existing MCP, health, Host and observation handlers preserved | 233 gateway tests including real CLIENT_TOKEN MCP and health | LOCAL PASS | dashboard-assets and existing protocol/observation suites | D4 deployed compatibility NOT RUN |
| AC08 / D1 | host/session/task live state と Fabric replica の authority が UI/API で区別される。 | Dashboard projections + separate component UI | Authority fields and retained/live badges | LOCAL PASS | D1 feca160; UI eefa61e; browser fixture | D4 |
| AC09 / D1 | offline/unavailable を empty/success/current に正規化しない。 | Liveness/replica components; cached UI staleness | Registry/probe/D1 failure tests; actual browser offline | LOCAL PASS | dashboard-scoping; frontend 24/24; browser fixture | D4 |
| AC10 / D1 | host inventory が server-side configured membership と owner-scoped replica metadata で構成され、`readOnlineHosts` は liveness overlay に限定される。 | dashboardMembership / dashboardRegistryLiveness / scoped replica reads | Malformed membership, own-property IDs, D1 scope tests | PASS | feca160; dashboard-scoping.test.mjs | — |
| AC11 / D1 | offline host が registry prune / page reload 後も識別できる。 | Configured inventory independent of pruned registry | Registry absence and prune fixture | PASS | dashboard-scoping.test.mjs | — |
| AC12 / D1 | availability (`online` / `offline` / `unknown`) は live route の有無のみを表し、判別不能な接続履歴や切断理由を断定しない (ケース A / B に未確認の履歴を付与しない)。 | Probe-confirmed online / complete absence offline / failure unknown | Registry and individual probe errors | PASS | dashboard-scoping.test.mjs | — |
| AC13 / D1 | host の接続 / 同期履歴は証拠が存在する場合のみ表示され、証拠なしは `unknown`、履歴取得元の失敗は `unavailable`。`configured_only` は情報源属性であり「未接続」の意味を持たない。 | Connection history evidence separate from replica sync | Absent source / failed registry history projection | PASS | dashboard-scoping.test.mjs | — |
| AC14 / D1 | configured membership 認可が host 一覧と host 配下の直接参照に一貫して適用され、stale D1 row からの復元表示をしない。 | Membership check precedes host and D1 reads on every nested route | Removed host direct session/task/context/timeline | PASS | dashboard-scoping.test.mjs; UI membership removal browser | — |
| AC15 / D1 | unknown / offline / unavailable と evidence 属性を混同しない。 | Availability separate from configured_only/source attributes | Inventory projection tests + text rendering | PASS | feca160; frontend 24/24 | — |
| AC16 / D2 | pending interaction は `runtime_owner` producer または `host_remote_observer` が生成し task metadata に保存された bounded summary を `task_list` が読み取る経路で表示され、欠落 field を「なし」と表示しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC17 / D2 | pending summary は `summary_revision` / `observed_at` / `producer_kind` / `producer_epoch` / `expires_at` を持ち、期限切れ・一部 endpoint 失敗・observer 停止を「対話待ちなし」と表示しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC18 / D2 | task status / revision 非依存の summary 変更が UI に反映される。 | Independent task/summary revision and epoch merging | Same task revision with newer pending summary; stale responses | LOCAL PASS | frontend 24/24; actual browser fixture | Host producer verification pending |
| AC19 / D2 | `task_list` / dashboard GET だけでは task/runtime の spawn・resume・reconciliation・answer が発生しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC20 / D2 | Devin Cloud の summary は D2 の Host-side read-only observer が更新し、dashboard GET / task_list / browser access を契機に起動・取得しない。 | CloudPendingObserver + SessionSupervisor independent 5-second lifecycle | Actual supervisor loop discovers retained tasks after tool return | LOCAL PASS | 36 Cloud + 32 supervisor tests; source review | D4 Host update NOT RUN |
| AC21 / D2 | 観測所有権 (`producer_kind` / `producer_epoch`) と実行所有権を混同せず、旧 owner の遅延結果を拒否する。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC22 / D2 | Cloud observer は binding 済み remote session の status read のみを行い、task status / operation receipt / backend generation / remote session binding を観測の都合で変更しない。 | Dedicated bounded typed remote status reader and metadata-only writer | None → pending → none; state/binding/receipt invariance | LOCAL PASS | devin_cloud::tests 36/36 | D4 |
| AC23 / D2 | 観測所有権は `observer_owner_id` / 永続 `producer_epoch` / 15 秒 lease を同一排他区間で比較更新し、acquire・renew・expire・takeover・publish・release の許可条件と保存内容が一意に決まる。 | Process mutex + flock; schema-3 persistent epoch CAS; 15-second lease | Cross-process lock, expiry/takeover/delayed publish/renew/release | PASS | 36 Cloud tests; schema migration and overflow fixtures | — |
| AC24 / D2 | 観測 metadata 更新は task `updated_at` / `created_at` / status / revision / generation / receipts / binding / owner / scope / org_id を変更しない。 | save_metadata_locked excludes activity timestamp and retention pruning | Exact protected-field comparisons after acquire/renew/publish/release | LOCAL PASS | Cloud/OpenCode metadata invariance fixtures | Codex/ACP final suite pending |
| AC25 / D2 | Host / observer 停止時、summary は期限切れとして扱うが remote task は停止しない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC26 / D2 | process restart 後、保存済み summary を現在の owner・接続状態・鮮度の確認なしに heartbeat だけで fresh に戻さない。 | Pending | Issue §13 regression cases | NOT RUN | Pending | Pending implementation / verification |
| AC27 / D1 | task list の backend-level unavailable が表示可能。 | Per-backend unavailable/partial status with retained rows | Backend failure/skipped/truncation fixtures | LOCAL PASS | frontend 24/24; dashboard API tests | D4 |
| AC28 / D3 | context freshness / gap / degradation が表示可能。 | Scoped bounded context components and independent freshness/status UI | Resolver failures/degraded journal; partial component fixtures | LOCAL PASS | 233 gateway tests; 24 frontend tests; actual browser | D4 |
| AC29 / D3 | timeline は sanitized replicated observation の bounded projection のみ。 | D1 owner/host/session allow-list projection; scoped incremental cursor | Raw/secret fixtures; cross-scope/tampered cursor; migrated SQLite SQL | PASS | dashboard-scoping.test.mjs; dashboard_schema_sqlite.py | — |
| AC30 / D1 | prompt/stdout/stderr/argv/environment/credential/raw evidence を dashboard data に追加しない。 | Allow-listed Host/task/context/timeline projections | Hostile raw/secret fixtures; XSS text rendering | LOCAL PASS | API/scoping/frontend + browser tests | Final integrated review pending |
| AC31 / D0 | dashboard API は no-store + same-origin + strict security headers。 | `dashboardResponse` on success/error/redirect/unknown/method paths | No CORS; no-store/CSP/nosniff/same-origin headers | PASS | Handler + real workerd response assertions | — |
| AC32 / D0 | dashboard assets は third-party runtime/CDN に依存しない。 | Plain local ES module/stylesheet assets | Review imports/resources and asset response contents | PASS | `gateway/assets/dash/`; real asset routing | — |
| AC33 / D1 | `npm test --prefix gateway` PASS。 | Pinned tooling with real workerd tests | npm test --prefix gateway | PASS (checkpoint) | 233/233 integrated gateway tests | Rust integration gates pending |
| AC34 / D4 | `wrangler deploy --dry-run` PASS。 | Same Worker assets binding; worker-first; no fallback | `npm run deploy:dry-run --prefix gateway` | PASS (integrated local) | Pinned Wrangler `4.142.0`; `/tmp/fabric-final-dryrun.log` | D4 deployed gate remains blocked |
| AC35 / D4 | deployed Zero Trust E2E を記録する。 | Candidate deploy/Host update rerun below | Deployed Zero Trust edge + Worker + Host acceptance | BLOCKED / NOT RUN | Pre-candidate edge 401 is explicitly separate | No approved candidate deploy/Host update; candidate browser session not established |
| AC36 / D1 | `git diff --check` PASS。 | Packet-scoped diff checks | git diff --check | PASS (checkpoint) | Each committed packet checked | Final full-tree check pending |

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

## Dashboard UI checkpoint

Integrated UI packet `4fd088f` passed 24 focused frontend tests. The committed assets passed all six fixed VLMKit gates (integrity, copy, scroll, handlers, interactions, breakpoint sweep), with zero suspect findings. Warnings remain for one contrast finding, unprobed event inventory, and unchanged fixture clicks. An actual Playwright browser separately verified hostile labels stay text, offline states become retained/stale, confirmed membership removal clears selected data even during replica failure, URL selection is preserved, and no page errors occur. The deterministic loopback fixture is not Access or deployed E2E evidence. Safe reproduction: `gateway/test/dashboard-fixtures/README.md`; screenshots remain owner-local.

## Additional local evidence

The integrated gateway suite passed 233 tests; pinned Wrangler dry-run passed with the same Worker and assets binding. Both migrated-SQLite schema checks passed. Cloud observer tests passed 36/36, supervisor tests 32/32, OpenCode tests 44/44 with one existing Host-only gate ignored. Final parallel Codex verification exposed a hang after readiness hardening; it is under repair and is not counted as a pass. Final Clippy exposed new lint failures under Rust 1.98; those are also under repair.

A scoped mutation run against the committed common summary implementation caught all three selected mutants (semantic-change guards true/false, and expiry comparison); zero survived or timed out. This is limited to common summary semantics, not a full D2 mutation claim. The 1,024 generated revision/freshness sequences and cross-process Host concurrency checks supplement backend ownership fixtures. Owner-local logs and screenshots contain synthetic fixtures and are not committed.
