# Agent backend: Devin CLI `devin acp` (ACP stdio JSON-RPC) delegation

## Status

open / implementation proposed

Model: coordinator decision (f4ah6o)
Created: 2026-09-23 (Asia/Tokyo)
Roadmap: `issues/ROADMAP-20260916-agent-mode-main-only.md`
Umbrella: `issues/open/20260922-agent-server-backends-cli-deprecation.md`

## Decision

Devin CLI を第三の server 系 delegation backend として追加する。transport は `devin acp`
(Agent Client Protocol over stdio JSON-RPC)。子プロセス spawn + JSON-RPC の構造は
Codex app-server adapter と同型であり、Temote の task 契約 (durable record,
idempotent operation receipt, typed control, scoped evidence, runtime lease) を
そのまま適用する。

- Devin: `devin acp` (stdio JSON-RPC、ACP `initialize`/`session/new`/`session/prompt`/`session/cancel`)
- 既存: `codex app-server --stdio` (`codex_task_*`)、`opencode serve` (`opencode_task_*`)

ACP はオープン規格 (https://agentclientprotocol.com/) であり、codex app-server の
vendor 独自 `thread/*`/`turn/*` より wire 面が小さい。`devin acp` はローカルエージェントを
駆動する経路であり、Devin Cloud session (API v3) とは別系統 — 本 issue の対象はローカル ACP のみ。

## Motivation / context

- 2026-09-23: Devin CLI に codex app-server / opencode serve 相当の仕組みがあるか調査 (coordinator 依頼)。`devin acp` が stdio JSON-RPC の同型経路であることを docs で確認 (https://docs.devin.ai/cli/reference/commands#devin-acp)。
- umbrella issue の方向性 (server 系優先) と整合: argv 統合 (`codex exec` / `opencode run`) は legacy 化中であり、新規 backend は server 系のみ追加する。
- `devin -p` / `devin --cloud` は本 issue の対象外 (one-shot argv / cloud session)。

## Design items

1. **ACP ownership**: scope 毎に `devin acp` を spawn・管理する。initialize → `session/new` → `session/prompt` の最小経路。`src/codex_app_server.rs` の ownership / lease / receipt / evidence 契約を再利用する。auth は `devin auth login` 保存資格情報 or `WINDSURF_API_KEY` or ACP `authenticate` request。
2. **Control ops**: steer → 同一 `sessionId` への追加 `session/prompt`、interrupt → `session/cancel`。resume は ACP `session/load` (optional capability) が advertise された場合のみ受理し、未 advertise では `reconciliation_required` 相当で fail closed。
3. **Approval bridging**: agent→client の `session/request_permission` を `waiting_approval` task state に写像 (opencode serve の permission/question と同じ扱い)。Temote 側 approval class は `CodexAppServer`/`OpenCodeServer` と同じ tier (`Ask => Request, _ => Skip`)。
4. **Report bridging**: ACP の turn 完了 (`session/prompt` response `stopReason`) 後、最終 assistant message を Temote 側で既存の bounded report contract (status/summary/base_commit/changed_files/checks/...) に JSON 検証して載せる。usage / observed model は ACP の `session/update` notification (usage_update 等) が提供する範囲で取得し、無い項目は null 許容。
5. **Tools**: `devin_status` / `devin_task_start` / `devin_task_get` / `devin_task_control`。`network` feature のみで compile し `--no-default-features` を維持。`temote-mcp doctor` の delegation readiness に `devin` binary + credential チェックを追加。

## Open questions (要 live 検証)

- `devin acp` が advertise する capability (`session/load`, prompt 中の追加 prompt 可否) と `session/update` notification の具体的 payload — インストール済み binary での実測が必要。
- `devin acp` 側の workspace/cwd 拘束指定 (codex `workspaceWrite` 相当) の有無 — ACP spec では client 側が `session/new` の `cwd` を渡す形; agent 側の write 境界は agent 依存。
- model/effort 指定経路: `devin acp --model` は new-session default のみ。session 毎の model 変更は `session/set_mode`/`set_model` 系 capability 依存。

## Phases

1. `devin_status` / `devin_task_*` backend を codex_app_server と同じ task 契約で実装する。
2. Live acceptance matrix に parity 行を追加する (`devin acp`: status/task start/get/control, permission denial, interrupt, orphan-free)。
3. parity 実測後に採用判断。primary 宣言は既存ルール (live evidence が揃うまで宣言しない) と同じ。

## Non-goals

- Devin Cloud (API v3) session 経路の delegation 化 — 別系統として必要になった時点で別 issue。
- `devin -p` one-shot argv 経路 (server 系優先の方針に反するため不採用)。
- `local_agent_run` tier への Devin 追加。

## Acceptance

- `devin acp` backend が既存 server backend と同等の idempotent / typed-control / scoped-evidence 契約で動作する。
- parity matrix の各項目が PASS、または棄却理由が記録されている。

## Progress

- 2026-09-23: issue 作成。Devin CLI の programmatic 経路調査完了 — `devin acp` (stdio JSON-RPC, ACP) を採用経路として決定。
- 2026-09-23: 実装完了 (PR 参照)。`src/devin_acp.rs` に Codex/OpenCode と同じ task 契約 (durable record / idempotent receipt / typed control / scoped evidence / runtime lease) で `devin acp` adapter を追加。`devin_status` / `devin_task_start` / `devin_task_get` / `devin_task_control` を公開 tool 化 (ungated module、`--no-default-features` 維持)。`session/prompt` の blocking response (`stopReason`) を turn 完了通知として扱い、`session/request_permission` を Temote-local approval 経由の `waiting_approval` に写像、`session/load` は `loadSession` capability advertise 時のみ受理 (未 advertise は fail closed)。`ApprovalClass::DevinAcp` を Codex/OpenCode と同 tier に追加、doctor の delegation readiness に `devin` binary + credential チェック追加、gateway contract / usage docs (en/ja) を同期。`devin` binary 未導入のため live parity 検証は未実施 — open question のまま。
- 2026-09-24: live 検証 (devin 3000.11.1 on macOS)。`devin_status`/`devin_task_start` が `initialize: deadline has elapsed` で必ず失敗 — actor が送る JSON-RPC message に `"jsonrpc":"2.0"` が無く、devin が返す `-32700` (`id:null`) parse error を pending にマッチできず 30s timeout していた。同 envelope を手動で流すと `jsonrpc` 有りで initialize ~10ms 応答 (protocolVersion 1, loadSession advertise, authMethods `devin-browser`)。修正は PR #43。`session/new` 以降の live parity は修正版 supervisor の再デプロイ後に再確認が必要。
- 2026-09-24: `devin acp --cloud` 対応を PR #43 に追加 (coordinator 依頼)。`devin_task_start` に `cloud` (boolean) を追加し、`devin acp --cloud` を spawn — stdio ACP を Devin Cloud ACP WebSocket に relay するため `auth login` account で hosted session が駆動できる (API key 不要)。`devin acp --cloud` では `--model`/`--agent-type` が無視されるため `cloud` との併用は拒否し、spawn 引数は `AcpSpawnSpec` に集約 (clippy `too_many_arguments` 対応)。`TaskRecord.cloud` (`#[serde(default)]`)・request fingerprint・task view・approval metadata・tool schema・gateway contract・usage docs (en/ja) を同期。`swe-2-max` 等の model 指定はこの経路では渡せない点に注意 — model 選択が必要な場合は `devin_cloud_task_start` を使う。cloud mode の live parity は修正版 supervisor の再デプロイ後に確認が必要。

## Related

- `issues/open/20260922-agent-server-backends-cli-deprecation.md` (umbrella)
- `issues/done/20260908-08-codex-delegation-dogfood-and-app-server.md` (Codex Phase D live gate)
- `issues/open/20260908-live-acceptance-matrix.md` (parity 行の追加先)
- https://docs.devin.ai/cli/reference/commands#devin-acp
- https://agentclientprotocol.com/
