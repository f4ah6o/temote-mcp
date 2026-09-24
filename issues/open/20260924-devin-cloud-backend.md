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
  未設定なら `/v3/self` から解決)、`TEMOTE_MCP_DEVIN_API_BASE_URL` (任意、`https://` 必須)
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
- 未実施: 実 credential での live 検証 (session 作成〜終了、`/v3/self` の org 解決、
  suspended → resume)。coordinator が `TEMOTE_MCP_DEVIN_API_KEY` を用意した後に実施し、
  結果をこの issue に追記する。

## Open questions

- `max_acu_limit` の default を Temote 側で強制するか (現状は caller 指定のみ、未指定なら Devin 側 default)。
- hosted session が `waiting_for_approval` のとき、Temote-local approval console へ橋渡しするか
  (現状は task status として表面化するのみ。承認は Devin web/Slack 側で行う)。
- `repos` の secret-scanning / allowlist (現状は文字列長のみ検証)。
