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

## Related

- `issues/doing/20260908-08-codex-delegation-dogfood-and-app-server.md` (Codex Phase D live gate)
- `issues/doing/20260922-opencode-run-cli-compatibility.md` (argv 破損の動機)
- `docs/evaluations/opencode-session-resume-spike-20260912.md` (E14: serve deferred の根拠)
- `issues/open/20260908-live-acceptance-matrix.md` (parity 行の追加先)
- https://github.com/f4ah6o/unofficial-opencode-sdk-rs (OpenCode Rust client, pinned OpenAPI snapshot)
