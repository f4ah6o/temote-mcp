# Gateway federation の end-to-end readiness 診断を追加する

Status: partially implemented / Slice A and the read-only remote endpoint, Access, and host-registration probe are implemented, including explicit `not_registered`/`lease_expired`/`generation_replaced` classification; session availability remains not_checked and live Cloudflare acceptance is pending
Created: 2026-09-11
Updated: 2026-09-12
Priority: P1 operator diagnostics

## 概要

Cloudflare Worker/Durable Objects gateway を利用する前に、host 側設定、local supervisor、Access service token、Worker 到達性、host registration の不足を秘密値を漏らさず判別できる `doctor` 診断を追加する。

この issue は **診断のみ** を扱う。診断実行によって session、host lease、Durable Object state、Access policy、Worker route、credential を変更してはならない。

## 背景

Gateway の設定要件は [`docs/gateway.md`](../../docs/gateway.md) と [`docs/gateway.ja.md`](../../docs/gateway.ja.md) に記録されている。現行の `temote-mcp doctor` は `TEMOTE_MCP_GATEWAY_HOST_ID` が設定されている場合に、host ID、gateway URL、host token の存在、Access service-token の組み合わせ、local supervisor の control protocol、named-root 数を確認する。

既存の [`issues/open/20260908-live-acceptance-matrix.md`](20260908-live-acceptance-matrix.md) は実 Cloudflare 環境での multi-host acceptance を追跡するが、設定ミスを起動前に切り分ける operator-facing preflight とは目的が異なる。

## 問題

現行診断だけでは、次の状態を local configuration failure と remote deployment failure に分離できない。

- Worker の public route または `/mcp` endpoint が想定 hostname に割り当てられていない。
- Worker の per-host `HOST_TOKENS_JSON` に local `host_id` がない、または token が一致しない。
- Access service token が期限切れ、無効、または host-agent 用 policy に許可されていない。
- `gateway-agent` が起動していない、または Worker の active host lease として登録されていない。
- gateway endpoint は生きているが、対象 host に利用可能な session がない。

このため、ChatGPT などの remote MCP client から接続できない場合に、Worker route、Access、host agent、session のどこを直すべきか判断しにくい。

## 目標

明示的な gateway 診断を実行すると、秘密値や physical root path を表示せず、local readiness と remote registration の状態を段階別に確認できるようにする。診断は tool call、session mutation、yolo 昇格、host token の再発行を行わない。

## 対象外

- Gateway の routing、generation/lease、MCP tool contract の通常動作変更
- Cloudflare Access policy の自動作成・自動変更
- Worker secret、Access service token、Tunnel token の自動生成または永続化
- direct Temote ingress の single-host semantics の変更
- multi-host live acceptance 自体（既存の live acceptance matrix で追跡する）
- diagnostic failure を自動修復すること

## 診断 contract

診断結果は最低でも次の stage を分離する。

```text
local_config
local_supervisor
remote_endpoint
access_auth
host_registration
session_availability
```

各 stage は概念的に次のどれかを返す。

```text
ready | failed | unavailable | not_checked
```

`unavailable` / `not_checked` を `ready` と同義に扱わない。Cloudflare API credential がないなど、host 側から検証不能な項目はその事実を明示する。

Remediation は non-secret な短い operator action に限定し、token、cookie、assertion、physical root、raw response body を出力しない。

## Read-only remote protocol requirement

remote 状態確認には次の優先順位を使う。

1. 既存の read-only health/status contract で必要情報を安全に取得できるなら再利用する。
2. 不足する場合のみ診断専用の read-only protocol を追加する。
3. 通常の host `connect`、session mutation、lease renewal を「診断代わり」に実行しない。

診断 endpoint を追加する場合も、MCP client OAuth と host-agent bearer token の境界を混同しない。未認証で host inventory や lease detail を公開しない。

## Executable implementation slices

### Slice A — local readiness refactor only

- 現行 `doctor` の gateway local checks を stage 化する。
- host ID format、HTTPS origin、host token の存在、Access credential pair、supervisor control protocol、named-root count を個別 result にする。
- secret 値/physical root path が stdout/stderr/JSON/error に出ない sentinel tests を追加する。
- remote network call は追加しない。

**Done when:** local configuration failure と local supervisor failure を deterministic に区別でき、既存 `doctor` behavior を壊さない。

### Slice B — remote endpoint + Access classification

- fake endpoint を使って TLS failure、DNS/connection failure、unexpected endpoint、Access rejection、authenticated reachability を分類する。
- endpoint が direct Temote か gateway Worker かを、明示的な response identity/readiness metadata で判別する。単なる HTTP 200 や body heuristic に依存しない。
- credential 値を response/log に残さない。
- route が host から確認不能な場合は `not_checked` とする。

**Done when:** local readiness が green でも remote endpoint/Access が壊れている状態を別 failure として返せる。

### Slice C — read-only host registration status

- Worker 側の既存 read-only status を再利用できるか確認し、不足する場合だけ narrow diagnostic contract を追加する。
- local `host_id` について `registered`, `lease_expired`, `generation_replaced`, `session_unavailable` を区別する。
- 診断で lease 作成/更新、agent connect、session start/stop を発生させない。
- gateway JS protocol tests と Rust fake-transport tests を追加する。

**Done when:** endpoint/Access が正常なケースで host-agent layer の failure を副作用なしに判別できる。

### Slice D — operator docs + live evidence

- `docs/gateway.md` / `docs/gateway.ja.md` に stage、exit semantics、remediation を同期する。
- 必要なら README には短い invocation のみ追加する。
- 実 Cloudflare 環境で read-only 診断を実行し、release/commit と結果分類を live acceptance matrix に記録する。
- secret 値は保存しない。

**Done when:** repository-local tests と credential-dependent live evidence が分離されている。

## 受け入れ条件

- [ ] gateway 診断が未設定、不完全、無効な host ID、無効な URL、supervisor unavailable を個別に報告し、非ゼロ終了する。
- [ ] host token、Access service-token、Worker secret map の値を出力せず、未設定・不一致・認証失敗を区別して報告する。
- [ ] endpoint が direct Temote か gateway Worker かを、explicit identity/readiness の確認結果として誤認なく示す。
- [ ] `gateway-agent` が未登録、lease expired、generation replaced、session unavailable の状態を切り分けられる。
- [ ] 診断は session、lease、tool、filesystem、Git、approval state を変更しない。
- [ ] remote verification を実行できない状態を `ready` と扱わない。
- [ ] Linux/macOS の unit/integration test で成功、設定不足、Access 失敗、remote 未確認、秘密値の非表示を固定する。
- [ ] `docs/gateway.md`、`docs/gateway.ja.md`、必要なら `README` の実行例と診断結果が同期する。

## テスト計画

Repository-local:

- `cargo test doctor`
- `cargo test --all-targets --all-features --locked`
- gateway Worker の read-only status/health protocol test
- fake endpoint を使った TLS、HTTP、Access 認証失敗、unexpected endpoint、未登録 host、lease expiry の決定論的テスト
- secret sentinel と physical root sentinel が stdout/stderr、JSON result、error path に現れないことのテスト
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `(cd gateway && npm test)`
- `git diff --check`

Live acceptance:

- 実 Cloudflare 環境で read-only 診断と authenticated `host_list` の結果を release/commit とともに `issues/open/20260908-live-acceptance-matrix.md` に記録する。

## リスク

remote status check を実装すると、Access や Worker API の一時障害を local configuration failure と誤表示する可能性がある。失敗分類を分け、診断不能を ready と扱わない必要がある。診断 endpoint を追加する場合は、host registration protocol の bearer token と MCP client の OAuth boundary を混同せず、認証なしの情報漏えいを避ける。

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Gateway federation の設定・接続状態を秘密値なしで確認できる `doctor` 診断を追加。

## 注記

- 2026-09-11: Gateway の設定要件は既存 docs に記録済みであることを確認した。今回の不足は設定仕様ではなく、Worker route、Access、host registration を横断した診断性である。
- 2026-09-11: `localmcp.obr-grp.com/*` の Worker route は direct Temote 復旧のため削除した。Gateway の再有効化は本 issue の診断・運用設計と分離する。

## Recommended next slice

**Slice A only.** まず既存 local `doctor` の結果を副作用なしで stage 化し、remote protocol 追加はその後にする。

## Implementation notes (2026-09-11)

Slice A landed on main (`src/doctor.rs`):

- `GatewayStage` / `GatewayStageStatus` model `local_config`, `local_supervisor`, `remote_endpoint`, `access_auth`, `host_registration`, and `session_availability`; `not_checked` maps to a warning, never to pass.
- Local configuration is reported per item (`host_id`, `gateway_url` origin validation via `gateway::normalize_gateway_url`, `host_token`, `access_service_token`), each with a non-secret detail.
- The `local_supervisor` check reports control protocol and named-root count only; physical root paths are never printed.
- `check_federation_readiness` performs no remote network call and still skips the supervisor probe when local config is invalid.
- Sentinel tests cover per-item classification, invalid host IDs/URLs, Access pair completeness, secret non-leakage, physical-root non-leakage, and the not-checked/ready distinction.

## Implementation notes (2026-09-12)

The first remote read-only slice is implemented:

- `/healthz` exposes explicit non-secret gateway identity/readiness metadata.
- Authenticated `POST /v1/hosts/status` reports only `registered`/`not_registered`, active generation, lease state, and `session_availability=not_checked`; it does not renew a lease or dispatch an MCP tool.
- `temote-mcp doctor` classifies endpoint identity, Access/host-token authorization, and host registration with bounded responses. Network failures become `failed`, authentication failures remain distinct, and unavailable session discovery is never treated as ready.
- Gateway tests cover the bounded status response. Credential-dependent Cloudflare route, Access, and live lease verification remain pending in the live acceptance matrix.

## Implementation notes (2026-09-12 classification slice)

- Remote health/status classification in `src/doctor.rs` now goes through pure helpers (`gateway_health_identity_ok`, `classify_gateway_host_status`) so endpoint identity and host registration are decided by explicit non-secret response fields rather than by HTTP status alone.
- An authenticated `404` distinguishes `host is not registered` from `host lease has expired`; a rejected `401`/`403` or an unexpected status now reports `host_registration=not_checked` instead of omitting the stage. No failure path maps to `ready`, and the successful registration detail may include the non-secret active `generation`.
- Deterministic unit tests cover gateway identity metadata, a registered host, a mismatched host ID, `not_registered`, `lease_expired`, unauthorized, and unexpected-status classification. These tests make no network calls.
- The gateway Worker now has a deterministic protocol test asserting that `/healthz` returns the explicit `identity=temote-mcp-gateway` and `readiness=ready` fields the Rust doctor classifies against, without requiring credentials.
- `session_availability` remains intentionally `not_checked`, and live Cloudflare route, Access, and lease verification remains tracked in `20260908-live-acceptance-matrix.md`.

## Classification slice residue

- The gateway read-only status contract reports the active generation but has no separate `generation_replaced` signal, so doctor cannot yet distinguish a replaced agent generation from a healthy one without a further read-only protocol addition.

## Implementation notes (2026-09-12 generation_replaced slice)

- The host-level gateway agent now owns a bounded, owner-only, non-secret local connection record under `<state>/gateway-agents/<host_id>.json` containing only `schema`, `host_id`, `generation`, and `updated_at`. It is written atomically after a successful `connect` and removed when the owning generation ends. No credential, token, or path is stored, and unsafe or malformed records are ignored.
- `temote-mcp doctor` reads that record read-only (no directory creation, `O_NOFOLLOW`, owner-only mode, bounded size) and compares it against the authenticated `/v1/hosts/status` `generation`. A newer remote generation is classified as `generation_replaced`, an older remote generation as `unavailable`, and an exact match as `ready` while naming the local generation.
- This resolves the `generation_replaced` residue without a remote protocol change: the classification is decided from the existing read-only status `generation` plus the non-secret local agent record. It distinguishes a superseded/stale local agent from a healthy one and never maps the state to `ready`.
- Deterministic Rust unit tests cover the record round-trip, removal on generation end, owner-only/symlink rejection, and the `generation_replaced`/`unavailable`/matched classifications. No network calls are made in tests.
- Limitation recorded honestly: a record left behind by a crashed agent whose gateway lease is still current and unadvanced cannot be distinguished from a live agent by read-only status alone; this remains an availability gap, not a false `ready` claim about registration identity.

## Classification slice residue (updated)

- `generation_replaced` is now classified from the non-secret local gateway-agent record. Remaining: `session_availability` is still intentionally `not_checked`, and live Cloudflare route/Access/lease evidence remains in `20260908-live-acceptance-matrix.md`.
