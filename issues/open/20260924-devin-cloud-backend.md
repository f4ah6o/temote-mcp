# Agent backend: Devin Cloud (API v3 hosted session) delegation

## Status

open / implemented, live verification pending

Model: coordinator decision (f4ah6o)
Created: 2026-09-24 (Asia/Tokyo)
Roadmap: `issues/ROADMAP-20260916-agent-mode-main-only.md`
Umbrella: `issues/open/20260922-agent-server-backends-cli-deprecation.md`
Related: `issues/open/20260923-devin-acp-backend.md` (local `devin acp`; 別系統)

## Decision

Devin Cloud を第四の delegation backend として追加する。transport は Devin API v3
(`https://api.devin.ai/v3/organizations/{org_id}/sessions`, Bearer 認証) の HTTPS
client であり、子プロセスは spawn しない。作業は Temote host ではなく Devin Cloud 側の
hosted session で行われる。`devin acp` (local CLI) と Devin Cloud (API v3) は
tool 名・approval class・task store・env 変数をすべて分離し、混同しない。

- tools: `devin_cloud_status` / `devin_cloud_task_start` / `devin_cloud_task_get` / `devin_cloud_task_control`
- approval class: `ApprovalClass::DevinCloud` (Ask では approval、Agent/Yolo では skip)
- task store: `state_dir()/devin-cloud-tasks` (schema v1、24h retention、64KiB record 上限)
- env: `TEMOTE_MCP_DEVIN_API_KEY` (fallback `DEVIN_API_KEY`)、`TEMOTE_MCP_DEVIN_ORG_ID` (任意、
  未設定なら `/v3/self` から解決)、`TEMOTE_MCP_DEVIN_API_BASE_URL` (任意、`https://` 必須)、
  `TEMOTE_MCP_DEVIN_CREATE_AS_USER_ID` (任意、service-user key の session を自分の user に帰属させる)
- feature gate: `network` (既存 `reqwest` を使用。`--no-default-features` では module 不在)

## Motivation / context

- 2026-09-24: coordinator 依頼「devin 対応を進めて devin cloud 利用できるように」。
  local ACP は Devin CLI binary が host に必要で、Cloud の hosted session はそれと無関係に
  API key だけで利用できるため、別 backend として追加する。
- API v3 の公式仕様 (https://docs.devin.ai/api-reference/v3/) を確認:
  session create/get/messages/terminate、`status` (`new`/`claimed`/`running`/`exit`/`error`/
  `suspended`/`resuming`)、`status_detail` (`working`/`waiting_for_user`/`waiting_for_approval`/
  `finished`/`inactivity`/`user_request`/quota/credit/payment/`error`)、structured output、
  `resumable`、`devin_mode`、`repos`、`max_acu_limit`。

## Implementation summary

- `src/devin_cloud.rs`: `CloudApi` (HTTP、redirect 拒否、4MiB response 上限、30s timeout)、
  `ApiError::{Rejected, Uncertain}` で「確定した拒否」と「効果不確定」を分離。
  durable `TaskRecord` + operation receipt/tombstone、owner/scope check、lifecycle permit、
  `derive_state` による remote status → `TaskStatus` 対応、structured output → final message
  の順で bounded report 抽出、terminal 時のみ scoped evidence 保存。
- `src/mcp.rs`: tool 定義・dispatch・approval detail/metadata (`scope: devin_cloud`、task input は
  omitted)。`src/activity/*`: `DevinCloud*` operation。`src/doctor.rs`: `delegation devin-cloud`
  (credential source のみ表示)。
- gateway: `protocol.js` 宣言、`routed-tools.json` / `public-tools.fingerprint` 再生成、
  test count 73 → 77。
- docs: `docs/usage.md` / `docs/usage.ja.md` に「Experimental Devin Cloud tasks」節。

## Verification

- Rust unit tests (fake API): create/bind、operation_id replay、rejected → `retryable_failed`、
  uncertain → `reconciliation_required`、remote status reconcile、structured/fallback report、
  steer/resume/interrupt、cross-session invisibility、status が secret を含まないこと。
- 実施済み (2026-09-24, service-user key `temote-mcp`, org `org-628d...`): `doctor` PASS、
  `devin_cloud_status` (`/v3/self` で org 解決、credential 値は出力されない)、`task_start`
  で hosted session 作成・bind・`running`、同一 `operation_id` + 異なる input は
  `OPERATION_CONFLICT` で fail closed、`task_get` で remote status reconcile、
  structured output から `completed` report 抽出、`interrupt` で remote DELETE → `interrupted`。
  live 検証で判明した実装差異を修正済み: Devin session は turn 終了後に `exit` せず
  `running`/`waiting_for_user` で idle するため、terminal structured report を
  `waiting_input` より優先する。
- 未実施: suspended → resume の live 確認 (suspended 状態を再現するタイミングが取れなかった)。
- 2026-09-24 追加検証 (temote-ebfe, temote-mcp@2026.9.19 on Mac host): `devin_cloud_status` は
  `TEMOTE_MCP_DEVIN_API_KEY` / `DEVIN_API_KEY` が supervisor の環境に未設定のため未構成と応答。
  利用には Mac host 側で API key を設定して supervisor を再起動する必要がある。
- 2026-09-24 `devin_mode` に `swe-2-medium` / `swe-2-high` / `swe-2-max` を追加
  (API v3 が受理する mode id)。この 3 値は SWE-2 の reasoning effort を選ぶもので、promo の課金/利用資格や priority/fast service lane を選ぶ値ではない。tool schema enum・`validate_devin_mode`・gateway `protocol.js`・contract snapshot / fingerprint を同期済み。
- 2026-09-26: SWE-2 の promo と priority/fast は upstream では effort と別軸として扱われることを確認。`swe_tier=promo|priority` を追加する。`promo` は requested SWE-2 mode を変えず、promotion 適用可否は upstream account state に委ねる。`priority` は local Devin CLI の `devin models list --format json` を bounded capability probe として使い、selected effort と `priority` / `fast` を含む account-visible SWE-2 UID が一意に存在する場合だけ effective mode として使う。UID suffix は推測生成しない。catalog が欠落・不正・失敗・曖昧なら Cloud API の session create 前に fail closed する。2026-09-17 snapshot と installed Devin CLI 3000.11.1 の bounded binary inspection では SWE-2 priority UID は未観測のため、live account で priority UID が公開されるまで `priority` は unavailable になり得る。

## Subscription 利用について (2026-09-24 coordinator 回答「サブスクリプションの範囲で使いたい」)

公式 docs (api-reference/authentication, getting-started/teams-quickstart) では、service user / PAT は
enterprise 専用ではなく Teams / standard organization の Settings > Service users で発行できる。
session はその organization の ACU / credit を消費するため、subscription の範囲内で利用できる。
自分の session として扱いたい場合は PAT を使うか、service-user key + `create_as_user_id`
(`TEMOTE_MCP_DEVIN_CREATE_AS_USER_ID`) を使う。

## Open questions

- `max_acu_limit` の default を Temote 側で強制するか (現状は caller 指定のみ、未指定なら Devin 側 default)。
- hosted session が `waiting_for_approval` のとき、Temote-local approval console へ橋渡しするか
  (現状は task status として表面化するのみ。承認は Devin web/Slack 側で行う)。
- `repos` の secret-scanning / allowlist (現状は文字列長のみ検証)。
