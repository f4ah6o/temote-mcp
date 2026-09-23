# Agent backends: server-primary (Codex app-server / OpenCode serve + Rust SDK) with staged CLI deprecation

## Status

open / direction decision and umbrella tracker

Model: coordinator decision (f4ah6o)
Created: 2026-09-22 (Asia/Tokyo)
Roadmap: `issues/ROADMAP-20260916-agent-mode-main-only.md`

## Decision

Codex および OpenCode の delegation / local-agent 経路を、長期的に server 系インターフェース優先とする。

- Codex: `codex app-server --stdio` (実装済み `codex_status` / `codex_task_start` / `codex_task_get` / `codex_task_control`)
- OpenCode: `opencode serve` HTTP + SSE。クライアントは `f4ah6o/unofficial-opencode-sdk-rs` (V1/current を primary、V2 preview は opt-in)

`codex exec` / `opencode run` の argv 統合面は、live parity 実測後に段階的に legacy 化 → 削除する。serve / app-server も同一 vendor binary 経由なので、廃止対象は binary ではなく one-shot subcommand 経路である。

## Motivation / context

- 2026-09-22: installed OpenCode CLI が `--pure` / `--dir` を拒否し broker が task 作成前に失敗 (`issues/doing/20260922-opencode-run-cli-compatibility.md`)。argv 契約の脆さの実例。
- server 系は steer / resume / interrupt / approval / event が取れ、Temote の task 契約 (idempotent receipt, typed control, scoped evidence) と一致する。
- `opencode serve` は一度 deferred された (spike E14: server lifecycle / port / auth / session ownership)。`OPENCODE_SERVER_PASSWORD` Basic Auth と Rust SDK の存在でその障害は解消可能。
- Codex app-server 側も wire 互換の変更実績がある (`workspaceWrite` → `workspace-write`、`effort` → `reasoningEffort`)。「server = 安定」とは断定せず、既存の contract-validation 方式を両 backend に維持する。

## Design items

1. **OpenCode server ownership (本丸)**: scope 毎に `opencode serve` を spawn・管理する。127.0.0.1 + 動的 port、インスタンス毎のランダム password を child env で注入、health gate、session 終了時 kill、orphan GC、reconnect 時の runtime lease。`src/codex_app_server.rs` の ownership / receipt / evidence 契約を再利用する。SDK は意図的に client-only (process launching 非含有) なので、この層は Temote 側の新規実装。
2. **V1 / V2 split**: primary 契約は stable な V1 current に載せる (`prompt_async` on busy session ≈ steer、既存 session への `prompt` = resume、`abort` = interrupt、`messages` = evidence/usage)。真の mid-run steer / `wait` が必要な箇所のみ V2 を同じ task contract の裏で opt-in にする。
3. **Report / permission bridging**: `--output-schema` 相当が無いため、最終 assistant message を Temote 側で JSON 検証して既存の bounded report 契約に載せる。permission/question event を approval console に橋渡しする。usage / observed model は messages API から取得し、CLI event stream (`observed_model=null` だった) より改善見込み。
4. **Sandbox tier の扱い**: `local_agent_run` の per-command bwrap one-shot は別 tier の製品価値。serve は host 側の永続プロセスなので、codex app-server と同様に agent-native permission + Temote approval の tier になる。bwrap 内に serve を常駐させる設計も検討余地はあるが、transport が TCP のため Temote からの到達手段が別途必要。
5. **`delegate` CLI UX**: 残して内部を task API に差し替える選択肢を取れる。local CLI 経路の sandboxed one-shot と server task は別の製品面として整理する。

## Phases

1. `opencode_task_*` (または unified `agent_task_*`) server backend を codex_app_server と同じ ownership / lease / receipt / evidence 契約で実装する。
2. Live acceptance matrix に parity 行を追加する (`codex exec` vs app-server / `opencode run` vs serve: structured report, usage, observed model/effort, permission denial, interrupt, orphan-free)。
3. parity 実測後に CLI 統合面を legacy 宣言 → 削除する。Codex 側も Phase D の live evidence が揃うまでは app-server を primary と宣言しない (採用基準と同じルール)。

## Non-goals

- `codex` / `opencode` binary の廃止 (serve / app-server も同一 binary 経由)。
- parity 未実測での CLI 経路削除。
- `local_agent_run` の sandboxed one-shot tier の即時廃止 (別途判断)。

## Acceptance

- server backend が `codex_task_*` と同等の idempotent / typed-control / scoped-evidence 契約で動作する。
- parity matrix の各項目が PASS、または棄却理由が記録されている。
- CLI 経路の legacy 化・削除が docs と code で一致している。

## Progress

- 2026-09-22: Phase 1 実装 — `opencode_status` / `opencode_task_start` / `opencode_task_get` / `opencode_task_control` を追加 (`src/opencode_server.rs`)。task ごとの `opencode serve` を 127.0.0.1 動的 port + instance random Basic-auth password + 隔離 data directory + `OPENCODE_CONFIG_CONTENT` の bounded permission で起動し、`unofficial-opencode-sdk` v1 API (`session create`/`prompt_async`/`abort`/`status`/`messages`、`permission`/`question` list) で駆動する。ownership/lease/receipt/retention/scoped-evidence 契約は codex_app_server と同一。deterministic な prompt `messageID` により start prompt の admission を判別し、crash window を `reconciliation_required` で表現する。module は `network` feature のみで compile し、`--no-default-features` build を維持。live parity (Phase 2) は `opencode` binary が利用可能な環境での実測待ち。
- 2026-09-23: Phase 2 live 検証 (host: Ubuntu VM, opencode 1.18.32, codex-cli 0.156.1)。実 MCP stdio 経路 (`temote-mcp supervisor` + agent session + approval console) で両 server backend を端まで通した。
  - `codex_status` → `codex app-server --stdio` spawn + initialize + `model/list` 実応答 (compatible:true, 実 model 一覧) → clean shutdown。
  - `codex_task_start` → `thread/start` + `turn/start` 受理、status `running`、`codex_task_get` が turn 完了を reconcile → status `failed` + bounded evidence。turn 自体は `401 Unauthorized` (api.openai.com、認証情報なし) で失敗 — wire contract は正常、model 実行のみ credential-blocked。
  - `opencode_status` → `opencode serve` spawn + health gate + `/provider` 実応答 (compatible:true, serve_version 1.18.32) → clean shutdown。
  - `opencode_task_start` → `session create` + `prompt_async` 受理、status `running`、`opencode_task_get` が reconcile → status `retryable_failed` (`APIError`, provider 未認証の model 実行失敗) — wiring 正常、model 実行のみ credential-blocked。
  - 修正した実害 bug 2件: (a) `opencode serve` 起動中の `health()` リクエストが server 側未応答のまま永久 pending になり health-gate deadline が効かず `opencode_*` 全系統が永久ハング → health probe に per-call timeout + 全 SDK 呼出に bounded timeout (`sdk_call`) を追加。(b) serve が `messageID` に `"msg"` 接頭辞を要求し `prompt_async` が HTTP 400 → deterministic `msg<uuid>` に修正。
  - credential 前提の parity 残項目 (structured report/usage/model 実行成功、permission denial、interrupt) は provider credential が無いため credential-blocked のまま — `issues/open/20260908-live-acceptance-matrix.md` の parity 行に部分 evidence を記録。
- 2026-09-23: CLI 経路の legacy/back-up 宣言 (coordinator 指示により parity 完全実測前倒し)。`temote-mcp delegate` / `temote-mcp codex delegate` の help 本文と `docs/development.md` に「server-backed `codex_task_*`/`opencode_task_*` が primary、one-shot argv は legacy fallback で削除予定」を明記。`local_agent_run` (per-command sandbox tier) は本指定の対象外 (Non-goal 維持)。argv 経路の削除自体は本変更に含まない。
- 2026-09-23: 再検証 (release `2026.9.12` 相当の main `cea66f2` debug build、同一 Ubuntu VM、codex 0.156.1 / opencode 1.18.32)。`codex_status`→`codex_task_start`→`codex_task_get`→`evidence_read` で rollout JSONL まで確認 (turn は `401 Unauthorized: Missing bearer or basic authentication` で credential 境界のみ失敗)。`opencode_status`→`opencode_task_start`→`opencode_task_get` で `status:retryable_failed`/`last_error:APIError`/`observed_model:opencode/big-pickle` を reconcile 確認 — wiring は両系統とも `.12` ビルドで正常、credential-blocked の状況は不変。
- 2026-09-23: 承認モデル変更 (explicit security-model change)。`ApprovalClass::CodexAppServer` / `OpenCodeServer` を `ask` 以外でも prompt 必須の tier (`Yolo => Skip, _ => Request`) から `GitNetwork`/`Integration`/`LocalStructured` と同じ tier (`Ask => Request, _ => Skip`) に移動。動機: `local_agent_run` (同じく coding agent を起動する構造化操作) は `agent` mode で承認不要なのに、後継の server-backed delegation だけが `agent` mode でも console 承認を要求し、remote/headless 利用 (public HTTP managed session は常に non-yolo + `agent` 既定、別起動の yolo session は public tool が拒否) で承認者不在に fail closed していた。delegation op は task/operation_id/model だけを受ける検証済み構造化操作で sandbox escape も argv も公開しないため、他の structured tier と同等の扱いが妥当。`ask` mode と `HostUnrestricted` の挙動は不変。child runtime 側 (app-server command/file-change、serve permission/question) の approval boundary は従来どおり別層で維持。
- 2026-09-23: `temote-mcp doctor` に delegation auth readiness チェック追加。`codex`/`opencode` binary の PATH/override (`TEMOTE_OPENCODE_BIN`) 解決 + credential 存在確認 (`$CODEX_HOME/auth.json` or `OPENAI_API_KEY`、`$XDG_DATA_HOME/opencode/auth.json`) を warn-level で報告 — binary 有り・credential 無し (401/provider 未認証で turn 失敗する状態) を事前検出できる。delegation は optional のため fail にはせず、turn 実行もしない。credential-blocked 状態での live 出力: `WARN delegation codex: binary=.../codex but no Codex credentials are visible` / `WARN delegation opencode: binary=.../opencode but no OpenCode credentials are visible`。
- 2026-09-23: OpenCode 2.x (`api/*`) contract 対応。動機: ユーザ Mac の opencode v2.0.11 は `/global/*` を持たず `opencode_status` が `/global/health` の decode error で落ちていた (codex 側は credentialed turn まで承認操作なしで動作確認済み)。`spawn_serve_once` に contract auto-detection を実装 — `global/health` 成功なら従来 v1 path、v1 が確定的 contract failure (Api error / decode error) を返した場合のみ `api/*` v2 adapter (`ServeClient::SdkV2`) へ fallback。transient error (起動中の refused/timeout) は v1 優先を維持し 3 回連続で v2 を probe — 両 contract を提供する server での境界 race により誤って v2 を選ばない設計。`TEMOTE_OPENCODE_SERVE_CONTRACT=v1|v2` で固定可。v2 message shape 差分を parser に吸収: `type`→role、`content`→parts、`model:{id,providerID}`→provider/model、`time.completed`/`error`/`id`/`tokens` は top-level でも読めることを確認。prompt id は v2 の `msg_` prefix 要求に合わせ `msg_<uuid>` に変更 (v1 の `msg` prefix 要求も満たす)。`serve_v2_contract_end_to_end` で 1.18.32 (`/global/*`+`/api/*` 両方) に対し v2 強制・Auto 共に実 turn (admission→assistant message→`content` text→`model` object→`time.completed`) まで検証。

## Related

- `issues/doing/20260908-08-codex-delegation-dogfood-and-app-server.md` (Codex Phase D live gate)
- `issues/doing/20260922-opencode-run-cli-compatibility.md` (argv 破損の動機)
- `docs/evaluations/opencode-session-resume-spike-20260912.md` (E14: serve deferred の根拠)
- `issues/open/20260908-live-acceptance-matrix.md` (parity 行の追加先)
- https://github.com/f4ah6o/unofficial-opencode-sdk-rs (OpenCode Rust client, pinned OpenAPI snapshot)
