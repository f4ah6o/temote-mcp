# Temote を local / remote agentic development harness へ再構成する

Status: open / umbrella tracker (polished; implementation not started)
Execution unit: one bounded child packet per run (small-model implementation guide below)
Created: 2026-09-24 (Asia/Tokyo)
Updated: 2026-09-24 (Asia/Tokyo) — current `main` baseline / gh-git / gh-stack facts verified; scope / responsibility boundaries revised per PR #47 design review; user priorities: approve-free agent mode, caller-location independence, no local main
Baseline inspected: `ba4c51c` (`main`, after PR #46 delegation-only tool surface)
Roadmap: `issues/ROADMAP-20260916-agent-mode-main-only.md`
Related:

- `issues/open/20260922-agent-server-backends-cli-deprecation.md` (server-primary backend umbrella)
- `issues/open/20260923-devin-acp-backend.md`
- `issues/open/20260924-devin-cloud-backend.md`
- `issues/open/20260925-observation-context-memory-plane.md` (high-priority head-independent observation / context / memory plane)
- `issues/open/20260925-vcs-transaction-jj-first.md` (high-priority VCS transaction / jj-first evaluation)
- `issues/open/20260925-v2-vcs-workspace-contract.md` (backend-neutral VCS/workspace contract after V1)
- `issues/done/20260916-managed-worktree-session-integration.md`
- `issues/done/20260916-agent-mode-git-broker-gh-git-integration.md`
- `f4ah6o/gh-git` (repository-scoped GitHub identity extension)
- `github/gh-stack` (official stacked PR extension)

## Goal

`temote-mcp` を、MCP server を中心とした実装から **local / remote 両対応の agentic development harness / development orchestrator** へ段階的に再構成する。

最終的には product / binary の中心名を `temote-mcp` から **`temote`** へ寄せ、MCP は Temote の transport / frontend の一つとして扱う。

この issue は umbrella tracker であり、直接実装しない。既存の server-backed agent delegation、managed worktree、gh-git integration を捨ててやり直すのではなく、現在の `main` から責務を整理して移行する。各 Phase は下記 Implementation guide に従い、入力・出力・手順・検証・完了条件を埋めた小さい child issue (`issues/open/` または `issues/polished/`) に切り出してから着手する。

## Top-level requirements (user-confirmed)

以下の 4 要件を設計・実装順序・受入判定の最上位に置く。ここで定めるのは target contract であり、下記 current baseline の実装済み事実とは区別する。

1. **yolo を使わず、極力 approve-free な agent mode。** repository / workspace / network / 操作範囲について既に与えられた許可を task に引き継ぎ、その範囲内の準備・実装・テスト・許可済み commit / push / PR 作成では操作ごとの再承認を要求しない。delivery という分類だけで毎回承認にしない。権限拡張や未許可の破壊的操作は明示的に扱い、sandbox・秘密情報保護・他作業の保護を維持する。
2. **指示役が cloud / local のどちらでも同じように使える。** authenticated caller が同じ権限を持つ場合、transport によって task の操作能力・承認方針・状態参照が変わらない。cloud から開始した task を local から追跡・制御でき、その逆もできる。指示役の場所と agent の実行場所 (host-local / hosted) は別の軸として扱う。
3. **独立して遅れ・未統合 commit の蓄積・分岐が生じる local main を持たない。** 新規 managed repository は bare repository store + task worktrees を標準とし、local `main` を作業・統合・追従のために維持しない。基準は fetch で確認した `origin/main` とその commit。既存 checkout は勝手に移動・削除・reset せず、明示的な移行契約で保全する。
4. **指示役を替えても context / knowledge を失わない。** coding agent に memory 管理を要求せず、Temote の共通 orchestration 境界で instruction / execution / evidence を自動観測する。raw observation と derived knowledge を分離し、専用 worker が非同期に整理する。次の head は authorized Context Resolver から provenance 付きの relevant context を取得する。hidden chain-of-thought や Temote 外の全 transcript を収集する設計にはしない。

### Agent-mode authorization contract

- Temote core は許可の scope / operation class / 有効性を共通に検証する。許可済み操作を client 接続・transport 切替・子操作への分割だけを理由に再承認しない。失効や scope 変更時は再評価する。
- `agent` は上記の範囲で prompt-free、`ask` は既存の対話承認方針を維持する。通常フローの成立に `yolo` を要求しない。
- repository / workspace 管理、environment preparation、delivery へ operation class を拡張する際も同じ方針を適用し、対応する child issue で AGENTS.md と policy tests を更新する。
- backend 自身の承認要件や外部サービスの認可は無効化しない。対応 backend では許可範囲を保つ設定へ写像し、残る入力待ち・承認待ちは capability / state として可視化する。Temote の prompt 削減と backend の制約を区別する。

## Core value and initial scope

Temote の中心価値は、**どの coding agent に任せても、適切な作業場所・権限・状態追跡・成果確認を提供する実行基盤** であること。

初期 orchestration は **caller が明示した task と依存関係の実行管理** に限定する。以下は初期 scope 外 (外側の ChatGPT 等 coordinator と内側の coding agent の判断責務に重なるため):

- 自律的なタスク分解
- 実装方針の決定
- backend / model の自動選択

## Current `main` baseline (verified 2026-09-24)

設計を現状に接地させるための事実。child issue はここから差分で書く。

### Delegation surface

- PR #46 以降、公開 MCP tool は 25 個: `session_start` / `session_list` / `session_info` / `session_stop` / `session_restart`、`poll_job` / `job_list` / `stop_job`、`evidence_read`、および 4 backend × `{*_status, *_task_start, *_task_get, *_task_control}`。
- `local_agent_run` / `dev_tool_run` / managed-worktree 系 MCP tool / 直接 shell・Git 操作 tool は削除済み。
- backend は 4 系統 (3 ではない):

  | backend | module | transport | tool prefix |
  | --- | --- | --- | --- |
  | Codex | `src/codex_app_server.rs` | `codex app-server --stdio` | `codex_` |
  | OpenCode | `src/opencode_server.rs` | `opencode serve` HTTP + SSE (`unofficial-opencode-sdk-rs`) | `opencode_` |
  | Devin (local ACP) | `src/devin_acp.rs` | `devin acp` (`cloud: true` で `devin acp --cloud`) | `devin_` |
  | Devin Cloud | `src/devin_cloud.rs` | Devin API v3 HTTPS (子プロセスなし、`network` feature) | `devin_cloud_` |

- 4 module はすでに同形の entry point を持つ: `status(session)` / `task_start(args, session)` / `task_get(args, session)` / `task_control(args, session)` (`args: &serde_json::Value`, `session: &config::Session`)。dispatch、approval metadata 生成、承認 (`authorize_*_operation`) は `src/mcp.rs` `call_tool` 側にあり、backend module は承認済み前提で呼ばれる。**Phase A の抽出点はこの seam** であり、task store / receipt / evidence は backend ごとに別実装のまま。
- `src/delegation/` は旧 one-shot `codex delegate` / generic delegate CLI 系で、server-backed task lifecycle の core ではない。名前が紛らわしいので orchestration core を置く場所として流用するかは Phase A child で判断する。

### Local control plane

- supervisor はすでに owner-only Unix socket を持つ: `config::supervisor_socket_path()` (= `socket_dir()/supervisor.sock`)、親 dir `0700`、socket `0600` (`src/session_control.rs` `run_supervisor`)。
- protocol は `ControlRequest` enum (serde tagged) で、現状は session lifecycle / approval / permission / upgrade / console 系のみ (`Ping`, `Approval`, `Start`, `StartLocal`, `List`, `Info`, `Stop`, `Forget`, `Gc`, `Restart`, `Permission*`, `Upgrade`, `AttachConsole`, …)。**task 系 request は無い**。
- 明示的な protocol version field は無い。task 系追加時に versioning を入れるかは Phase B child で決める。
- Windows named pipe 実装は無い (Unix socket 前提)。Windows transport は本 issue の初期 scope 外とし、必要になった時点で別 issue にする。
- CLI (`src/cli.rs` `Command`) に `task` サブコマンドは無い。

### Managed worktree

- layout は `<src-root>/<repo>` (canonical primary checkout、**src root 直下 1 階層のみ**) と `<src-root>/worktrees/<repo>/<name>` (managed)。`ManagedRepository::resolve` は `<src-root>/<owner>/<repo>` のような nested checkout を fail closed で拒否する。
- repository identity は directory 名 (`<repo>`) のみで owner を含まない。`--repo f4ah6o/example` のような owner 付き指定を受けるには、owner 衝突時の扱いを含めた mapping が必要。
- worktree は `Primary` / `Managed` / `Legacy` に分類され、Legacy は報告のみで adopt / move / delete しない。
- reservation (`acquire_*_worktree_reservation*`, `acquire_worktree_admission`) は `src/managed_worktree.rs` にあり、supervisor / session_control から使われる。
- 公開 tool から worktree を作る経路は PR #46 で消えている。現状 task の workspace = session の canonical scope。

### gh-git

- 現状の surface は Git passthrough (`gh git <git args>`) と binding 管理 (`bind` / `unbind` / `binding status [--json]` / `accounts` / `doctor` / `env` / `shell-init`) のみ。`workspace` namespace は無く、`gh git worktree ...` は素の `git worktree` passthrough。
- binding は repository-local config (`github.identity`, `github.host`, `user.name`, `user.email`) に書かれ、`git rev-parse --git-path config` 経由なので linked worktree からも common config として共有される。
- tokenless `GH_CONFIG_DIR` profile は `<common-git-dir>/gh-git/gh-config/` に作られる (`internal/gitconfig/git.go` `Paths`)。
- **既知の gap**: shell hook (`internal/shell/shell.go`) は profile を `git rev-parse --git-path gh-git/gh-config` で探すが、linked worktree ではこれが `<common-git-dir>/worktrees/<name>/gh-git/gh-config` に解決される (`gh-git` は Git の common path 一覧に無いため)。結果として **linked worktree 内では hook が profile を見つけられず、direct `gh` command が global active account に fallback する**。scratch repo で再現確認済み (`git worktree add` 後に `--git-path gh-git/gh-config` → `.git/worktrees/<name>/...`, `--git-common-dir` → `.git`)。Section 6 の前提条件として gh-git 側で `--git-common-dir` 基準に直す必要がある。

### gh-stack (official, `github/gh-stack`)

- `gh stack link <branch-or-pr>...` は **local tracking state を読み書きしない** (jj / Sapling / git-town 等の外部 branch 管理向け)。引数は bottom → top 順。
- ただし side effect は大きい: branch を remote に **push** し、PR が無ければ **作成**、base chain がずれていれば **base を修正**、GitHub 上の Stack object を作成 / 更新する。update は additive only (既存 PR を stack から外さない)。flags: `--base`, `--open`, `--remote`。
- `gh stack view --json` は local tracking (`.git/gh-stack`) 前提のため、`link` のみで運用する場合は machine-readable state を PR / Stack 側 (GitHub API) から取る必要がある。
- `init` / `add` / `checkout` / `rebase` / `sync` は checkout / rebase を伴い、worktree ownership と衝突しうる。

## Current direction

コード操作は Temote 自身が shell / Git broker で代行するのではなく、server-mode の coding agent に委譲する (上表の 4 backend)。Temote 自身が各 Git command を emulation / proxy することは中心責務にしない。

### Responsibility boundaries

| 層 | 責務 |
| --- | --- |
| gh-git | repository-scoped identity、repository / worktree の作成・検査等の Git primitive |
| Temote workspace | task への割当、reservation / 書込み排他、回収条件 |
| Temote environment | vp / Cargo 等の準備、ready-state、cache policy |
| agent backend | 実装・調査・テストの実行 |
| delivery adapter | branch / PR の提出と状態確認 |

**AGENTS.md からの境界変更**: 現行 AGENTS.md は「machine operations は agent に委譲する」としている。本 issue は workspace / environment / delivery の **管理操作** を Temote の固定 adapter が担い、実装中の **コード操作** は引き続き agent が担う、という区別を導入する。各 child issue (Phase F / C / D / E) は、どの操作がどちらに属するか、および固定 adapter に適用する実行権限 (sandbox profile / network / approval class) を明示し、AGENTS.md を同じ packet で更新する。

## Target architecture

```text
                         Temote
                           |
                    Orchestration Core
                           |
   (approval / receipt / reconcile / evidence / state)
                           |
                  observation recorder
                           |
                  observation / context
                           |
     +-------------+-------+-------+-------------+
     |             |               |             |
 workspace    environment        agent        delivery
     |             |               |             |
   gh-git     vp / Cargo /   Codex / OpenCode /  gh (single PR)
 (Git prim.)    sccache      Devin ACP /        gh-stack (stack)
                             Devin Cloud

frontends / transports:
  - Local CLI / Desktop
  - MCP
  - HTTP
  - Gateway
```

Temote の中心責務:

- caller が明示した task / 依存関係の実行管理 (分解・方針・backend 選択は caller 側)
- caller が選んだ agent backend の server lifecycle
- workspace の割当 / reservation / 書込み排他
- environment preparation と ready-state
- approval / receipt / reconcile / evidence / activity
- execution / verification / delivery state の記録
- delivery (単独 PR / stacked PR) の提出と状態確認
- transport-independent な instruction / execution observation と、head 切替用 Context Resolver (整理は専用 worker。planner にはしない)

Devin Cloud は Temote host 上の workspace を使わない (hosted session 側で repo を clone する)。workspace / environment / reservation 系の契約は host-local backend (Codex / OpenCode / Devin ACP local) に適用し、Devin Cloud は task / evidence / delivery 契約のみを共有する。

## 1. Extract an orchestration core from MCP

現在 `src/mcp.rs` の dispatch と 4 backend module に分散している server-backed task orchestration を transport-independent core に引き上げる。

概念 API:

```text
Orchestrator
  task_start(backend, ...)
  task_get(...)
  task_list(...)
  task_control(...)
  task_reconcile(...)
  workspace_ensure(...)      # Phase C 以降
  delivery_submit(...)       # Phase E 以降 (単独 PR / stack)
```

frontend は薄い adapter にする。

```text
LocalControlAdapter
McpAdapter
HttpAdapter
GatewayAdapter
```

既存の公開 MCP tool 名 (`codex_task_start` 等) は compatibility surface として維持し、内部で `task_start(backend = Codex | OpenCode | DevinAcp | DevinCloud)` へ normalize する。backend 固有の入力 (Devin Cloud の `devin_mode` / `repos` / `max_acu_limit`、Devin ACP の `cloud` など) は backend-specific options として型付きで保持し、共通 contract に無理に寄せない。

AGENTS.md の delegation invariant (typed task contract のみ、executable / raw argv / env block / network policy / scope 外 path を受けない、`operation_id` 必須、detail は bounded evidence のみ) は core の不変条件として移す。

### Core は呼び分けではなく共通の実行保証を持つ

最初の packet は enum dispatch による behavior-preserving extraction でよいが、到達点の core は以下を **共通の入口** で扱う:

- approval / permission mode 判定 (現 `authorize_*_operation`)
- operation receipt / `operation_id` による重複防止
- 状態照合 (reconcile)
- evidence ownership (session / scope 束縛)

承認処理を `src/mcp.rs` に残したまま local frontend が backend module を直接呼ぶ構成にはしない。Phase B の前に承認経路を core へ移す。

### Backend capability 差を保持する

hosted execution (Devin Cloud)、resume 可否 (Devin ACP の `loadSession`)、入力待ち (`waiting_for_user` 等)、停止の意味 (interrupt vs terminate) は backend ごとに異なる。共通 contract はこれらを一律に扱わず、capability と backend-specific state を型として保持する。

task store は最初から全面統合しない。backend ごとの store を維持し、共通の参照 / index と状態契約から始める。

### Task / execution / verification / delivery を分ける

backend の `completed` は依頼の受入条件達成を保証しない。少なくとも以下を別の状態として持つ:

- **execution state**: agent execution の状態 (running / waiting_for_input / completed / failed / interrupted / unknown / reconciliation_required)
- **verification result**: 対象 revision に結び付いた検証結果。未実行は `not_run` であり PASS と扱わない
- **delivery state**: branch push / PR / stack の状態

入力待ち・状態不明・reconciliation required は success / failure に読み替えずに保持する。

大きな workflow engine は先に作らないが、将来の再実行や backend 変更に備えて **論理 task** と個々の **execution** を区別できる ID / record 構造を残す。

## 2. Add local orchestration without MCP

ChatGPT Desktop や native local client から Temote を利用する場合、MCP round-trip を要求しない local path を追加する。

例 (UX 案。flag 名は Phase B child で確定):

```sh
temote task start --local \
  --backend opencode \
  --session <session-id> \
  --operation-id <uuid> \
  --task "implement api"

temote task list --local
temote task get --local <task-id>
temote task steer --local <task-id> "testsも追加して"
temote task stop --local <task-id>
temote task reconcile --local <operation-id>
```

rename (Phase G) 前は現 binary 名 `temote-mcp task ...` で提供する。

`--local` の意味は **transport が local であることだけ**。

```text
--local != yolo
--local != unrestricted filesystem
--local != approval bypass
--local != sessionless
```

identity / workspace ownership / approval / credentials / evidence / activity policy は MCP / remote と共通にする。task は MCP と同じく session に束縛する。接続ごとの認証と session ownership の検証を行った上で、同じ権限を持つ cloud / local caller は同一 task / operation ID / evidence を扱う。指示役の場所だけを理由に追加承認や機能制限を設けない。

### Retry / reconciliation UX

`operation_id` の CLI 自動生成だけでは、応答消失後にコマンドを再実行したとき別 ID で二重起動しうる。Phase B の契約に以下を含める (具体形は child で確定):

- caller が `--operation-id <uuid>` を明示でき、同じ session / operation scope で同一 ID・同一 request fingerprint の再送は既存 receipt に基づく結果を返す。同一 ID で task / backend / options 等が異なる request は conflict として拒否し、副作用を再実行しない
- 自動生成時は送信前に ID を表示 / 保存し、再実行時に再利用できる
- 明示的な再照合 (`task get` / `task list` による再発見、または `task reconcile`) で、応答消失した操作の結果を確認できる

### Phase B acceptance scenarios

- local から開始した task を、同じ権限を持つ MCP client から同一 task として追跡・制御できる (逆も同様)。
- client 切断後も task を再発見でき、応答消失後の再試行で二重起動しない。
- supervisor / backend 再起動後に状態を照合できる。継続不能ならその事実を報告し、不明を成功・失敗に読み替えない。
- approval / permission / scope / evidence の条件が transport によって変わらない。

### Local control plane

CLI process に agent server を所有させず、既存 supervisor の owner-only control socket (`supervisor.sock`) に task 系 `ControlRequest` を追加する。

```text
Desktop / CLI
    |
 local control protocol (supervisor.sock)
    |
Temote supervisor
    |
    +-- task A -> Codex app-server
    +-- task B -> OpenCode serve
    +-- task C -> Devin ACP
    +-- task D -> Devin Cloud API
```

task 系 request 追加時に protocol version negotiation を入れる (現状 version field 無し)。Windows named pipe は初期 scope 外。

将来的な利用先:

- Temote CLI
- ChatGPT Desktop-like local integration
- native GUI
- VS Code / JetBrains integration

## 3. gh-git as the Git repository / worktree substrate

`f4ah6o/gh-git` を Temote 専用品にはせず、人間単独でも使える Git repository / worktree substrate として発展させる。task 割当・排他・環境準備は持たない (Responsibility boundaries 参照)。

責務:

- repository-scoped GitHub identity / credential routing (既存)
- repository store
- worktree lifecycle (作成 / 検査 / 削除の Git primitive)
- workspace inspection
- machine-readable JSON contract
- Git 側の workspace 準備 (branch / upstream / identity wiring まで)

言語別の依存・toolchain・cache 準備は gh-git に持たせず Temote environment 層が担う (Section 4)。gh-git と Temote の双方が開発環境管理を抱えることを避ける。外部の preparation tool を Temote environment から再利用することは妨げない。

候補 surface (gh-git の passthrough 名前空間と衝突しないよう `workspace` は gh-git 側の reserved namespace にする。`issues/open/20260916-passthrough-command-collision-policy.md` in gh-git と整合させる):

```sh
gh git workspace ensure f4ah6o/example \
  --branch feat/api \
  --name task-api \
  --json

gh git workspace inspect --json
gh git workspace list --json
gh git workspace remove task-api
gh git workspace prune
```

### Worktree-only / bare-first default for new managed repositories

新規 managed repository は **bare repository store + task branch ごとの worktree** を標準形とする。`repository != workspace` を構造として分離し、local `main` の checkout / commit / merge / pull を通常フローに持ち込まない。bare-first は後段の opt-in 機能ではなく、最上位要件を満たす初期 workspace 基盤に含める。

- store 作成時に remote-tracking ref の fetch 設定を明示し、`origin/main` を更新する。`refs/heads/main` の作成・維持を必要としない構成にする。bare 化だけでこの ref 契約を満たしたと見なさない。
- task 開始時に許可済み fetch を行い、取得した `origin/main` の commit と確認時刻を記録する。独立 task はそこから専用 branch / worktree を作る。stacked task は明示選択された親 branch / revision を直接の基点とし、stack の root が参照する `origin/main` も記録する。
- fetch 失敗時は最後に確認した commit / 時刻と freshness 未確認を返し、最新と扱わない。開始の可否は事前に定めた freshness policy に従う。接続できないことを yolo や scope 拡張で回避しない。
- 長期 task の再開時と delivery 前に再 fetch し、基点と現在確認できる `origin/main` の差を検査する。必要な追従を task branch 上で行い、変更後の revision に対して再検証する。remote の進行と競合しうるため「常に最新」とは保証しない。
- 統合は GitHub の PR 経由とし、merge 済みの結果を fetch する。local `main` に未提出 commit を積み、後で同期する運用は作らない。PR merge 自体も既存の許可と repository rules に従う。
- task branch の未統合 commit や意図した stack は local main の分岐とは区別する。未コミット変更・未提出 commit を自動破棄して追従や cleanup を成立させない。

### Existing checkout compatibility and migration

現行 managed worktree は canonical primary checkout (`<src-root>/<repo>`) と `<src-root>/worktrees/<repo>/<name>` を前提にする。この baseline は保全し、新規 managed store の標準とは分けて扱う。

1. Phase F で `RepositoryStore + Workspace` と remote-tracking ref / freshness の契約を共通コアと並行して設計する。既存 layout と owner/repo mapping は compatibility adapter で扱う。
2. gh-git に Git primitive と versioned JSON contract を置き、Temote はそれを利用して task ownership を管理する。
3. 新規 store を bare-first / no-local-main とし、Phase C の workspace 割当と組み合わせて初期 acceptance を通す。
4. 既存 repo は inspect して未コミット変更・未提出 commit・worktree / identity binding を報告する。既存 local main を reset / delete / 強制同期せず、移行を明示的に行うまで保持する。
5. 既存 checkout の保全を「no-local-main 移行完了」とは数えず、新規標準と legacy compatibility の状態を分けて報告する。

### Session / workspace mapping (Phase C の前提)

現状 task は session の canonical scope を workspace として使う。「task ごとの isolated workspace」を成立させるには、session と workspace の対応、同時 writer 数、session 停止時の扱いを先に決める。

初期案: **workspace ごとに session を持ち、同一 workspace の active writer を一つに制限する**。既存の path-scoped session / reservation と整合し、親 session の scope を広げずに済む。別案 (1 session が複数 workspace を持つ等) を選ぶ場合も、親 session の scope を暗黙に広げない方法を child issue に明記する。session 停止時に workspace を回収するか保持するかも同じ child で決める (uncommitted work は破棄しない)。

### Ownership boundary

- gh-git: Git 的な repository / worktree primitive
- Temote: task ownership / reservation / concurrent agent safety

他 task / 他 agent の worktree を勝手に remove / prune しない invariant、および Legacy worktree を adopt / move / delete しない invariant は Temote 側に残す。gh-git の `remove` / `prune` を Temote から呼ぶ場合も、Temote reservation を取得してからにする。

## 4. Environment preparation (Temote environment layer)

worktree が Git 的に存在するだけでは ready としない。

```text
Git ready
+ dependencies ready
+ toolchain ready
+ cache ready
= agent-ready workspace
```

environment preparation は Temote environment 層の責務とし、gh-git の Git 側準備の後段に置く。preparation の実行は coding agent への委譲ではなく Temote environment の固定 adapter が行う (caller は argv を指定できない。AGENTS.md の typed contract invariant と同じ)。network を要する install は既存 approval class / network policy に従う。

### JS / TS

Vite+ (`vp`) に寄せる。

```text
workspace ensure
  -> detect package.json / lockfile
  -> vp install --frozen-lockfile
  -> pnpm shared store / virtual store reuse
  -> ready
```

原則として `node_modules` 自体を sibling worktree 間で共有しない。

共有するもの:

- pnpm content store / safe shared cache

worktree-local:

- dependency view / links
- generated local state

`vp install` の flag 名 / pnpm store 共有挙動は Phase D child で pinned version に対して確認する (本 issue では未検証)。

### Rust / Cargo

依存 source/index/git checkout は通常の shared `CARGO_HOME` を利用。build artifact は全 worktree 共通 `target/` にしない。

```text
shared:
  CARGO_HOME
  sccache

workspace-scoped:
  CARGO_TARGET_DIR
```

ephemeral Temote workspace では必要に応じて:

```text
RUSTC_WRAPPER=sccache
CARGO_INCREMENTAL=0
CARGO_TARGET_DIR=<Temote environment managed cache>/<repo>/<workspace>
```

worktree cleanup 時に workspace-scoped target cache を回収可能にする。これらの env は backend spawn 時の child env (`src/child_env.rs`) 経由で渡し、caller 指定の env block は引き続き受けない。

## 5. Integrate official github/gh-stack for delivery

stacked branch / stacked PR 自体は Temote / gh-git で再実装しない。official `github/gh-stack` を delivery backend として利用する。

初期 integration は local tracking を持たない `gh stack link` を中心にする。

```text
delivery-selected       branches          GitHub
tasks

task-auth                main
  |                        |
task-api        ->      feat/auth    ->  gh stack link feat/auth feat/api feat/ui
  |                        |
task-ui                 feat/api
                           |
                        feat/ui
```

`task graph ~= branch graph ~= PR stack` は一般には成り立たない: 調査 task は branch を持たないことがあり、実装とレビューが同じ PR に関わることも、複数 task を一つの PR にまとめることもある。したがって task graph から **delivery 対象の branch とその依存関係を明示的に選び**、それを stack へ写像する。単独 PR も通常の delivery として扱い、stack は選択された branch 依存が 2 段以上ある場合の一形態とする。

gh-stack 統合は共通コア / local frontend の成立条件にしない。bare-first / no-local-main は初期 workspace 基盤 (Phase F + C) の必須条件とし、単独の core extraction は先行できる。

注意点 (verified):

- `link` は push / PR 作成 / base 修正 / Stack 作成を行う remote side effect であり、delivery operation class として検証する。`agent` では対象 repo / branches / 操作が既存許可に含まれる限り再承認しない。`ask` は対話承認を維持する。未許可の対象や操作まで拡張せず、部分成功・再試行も receipt と remote state から照合する。
- `link` は additive only。stack から PR を外す / 並べ替える操作は初期 scope 外。
- `link` 運用では `gh stack view --json` が使えないため、stack / PR state evidence は GitHub API から取得する。
- `gh` の identity は Section 6 の repository-scoped profile を使う。

初期段階では gh-stack の `init` / `add` / `checkout` / `sync` / `rebase` に worktree lifecycle の authoritative ownership を持たせない。

- Temote / gh-git: branch / worktree lifecycle
- gh-stack: GitHub Stack / PR base relation / stacked PR create/link/status/merge

## 6. Repository-scoped GitHub identity across worktrees

gh-git の repository-scoped identity を repository store / common Git configuration に保持し、すべての linked worktree と agent task で同じ GitHub account mapping を利用する。

```text
1 Git repository
= 1 GitHub identity
= N worktrees
= N Temote tasks
```

前提修正 (gh-git 側、Phase C より前): shell hook と `env` の profile 解決を `--git-path gh-git/gh-config` から `--git-common-dir` 基準に変え、linked worktree でも `<common-git-dir>/gh-git/gh-config` を選ぶようにする (上記 baseline の gap)。

Codex / OpenCode / Devin ACP server spawn 時は、Temote が `--git-common-dir` から profile を解決し、tokenless `GH_CONFIG_DIR` を child env に渡す (shell hook に依存しない)。

以下を避ける。

- global `gh auth switch`
- remote owner 名から GitHub login を推測
- raw token を task / evidence / activity / approval summary に出す
- `GH_TOKEN` / `GITHUB_TOKEN` を child env に継承させて binding を上書きすること

## 7. Rename / product boundary: temote-mcp -> temote

この restructuring の完了形では Temote の中心は MCP server ではない。

目標 UX:

```sh
temote task ...
temote workspace ...
temote stack ...
temote activity ...

temote mcp
temote serve
temote gateway-agent
```

`mcp` / HTTP / Gateway は transport adapter として扱う。

rename の影響範囲 (migration packet で扱う): binary / crate / cargo-dist package、`TEMOTE_MCP_` env prefix、state / socket dir、gateway worker 名と protocol contract、`skills/temote-mcp/`、README / docs / AGENTS.md の product name 規則、release workflow。

この issue では rename を即時実施せず、core/frontend separation が成立してから migration packet を切る。

## Proposed domain boundaries

```text
repository
workspace
environment
task / execution
verification
agent
delivery
transport
```

例:

```text
orchestration/
  task/
  execution/
  verification/
workspace/
environment/
agents/
  codex/
  opencode/
  devin_acp/
  devin_cloud/
delivery/
transport/
  local/
  mcp/
  http/
  gateway/
```

実際の Rust module 名は既存コードとの差分を見て別 packet で決める。

## Non-goals

- 現在の実装を最初から作り直すこと
- Codex / OpenCode / Devin の Git 操作を Temote が command ごとに proxy すること
- gh-git を Temote 専用 CLI にすること
- gh-stack を fork / vendor して独自 stacked PR implementation を持つこと
- sibling worktree の `node_modules` / Cargo `target` を雑に共有すること
- `--local` を unrestricted / yolo / sessionless mode にすること
- rename のためだけに compatibility を壊すこと
- 初期 scope での Windows named pipe transport
- Devin Cloud hosted session に host-local workspace / environment preparation を適用すること
- 自律的なタスク分解、実装方針の決定、backend / model の自動選択 (初期 scope)
- 大きな workflow engine を先に作ること
- gh-git に言語別の環境準備を持たせること
- 通常の agent mode を成立させるために yolo を要求すること
- 指示役が cloud / local であることだけを理由に機能・権限・承認回数を変えること
- 新規 managed repository の local main を作業・統合 branch として維持すること

## Suggested implementation packets

優先する到達点:

1. 共通コア抽出と既存 MCP 契約の維持 (Phase A)。repository-store / no-local-main 契約の設計 (Phase F) は並行して進める。
2. cloud / local 共通の task lifecycle と許可済み範囲の approve-free agent mode (Phase B)。
3. **high priority:** V1 実測で jj-first が viable と確認済み。V2 の backend-neutral VCS/workspace contract に従い、agent の explicit commit に依存しない workspace transaction model を実装へ進める。F2/F3 の git-worktree-only 実装は行わない。
4. **high priority:** A の共通 Task/Execution identity を使って Observation journal / deterministic Context Resolver (O1/O2) を並行実装する。backend ごとの個別 logger は作らない。Memory Worker (O3/O4) はその後に載せる。
5. 切断・応答消失・再起動・再試行の状態照合を検証 (Phase R)。
6. bare-first / no-local-main の新規 store (Phase F) と workspace 割当・書込み排他 (Phase C) を、V1/V2 の VCS backend decision に従って完成させる。
7. environment preparation (D)、delivery (E) を個別に追加し、各操作の既存許可を引き継ぐ agent mode を検証する。

依存関係: A → B → R。V0/V1/V2 は完了済み。V1 で jj-first viable を確認し、V2 で backend-neutral contract を固定した。次は V3 typed VCS adapter と backend-neutral F/C workspace implementation を進める。O0 は完了済みの設計 packet、O1 は A2/A3 の common identity 後に開始し、O2 → O3 → O4 と進める。O1/O2 は B/F/C と並行でき、D/E より優先する。C0 (gh-git identity fix) と F の generic repository/freshness 設計は独立に開始できる。F の新規 store 実装は C0 と V2 に整合させ、C 完了には R + F + C0 を必要とする。C → {D, E}。G (rename) は A + B + R + F + C 完了後。F は後回しの opt-in ではない。各 phase は複数の小さい child packet に分け、設計・契約の決定と実装完了を区別する。

### Phase A — core extraction

最初の child は dispatch / 承認経路の抽出と既存契約の維持に限定し、record / index 拡張は後続 child に分ける。

最初の packet (推奨): 4 backend の `status` / `task_start` / `task_get` / `task_control` を 1 つの `Backend` 抽象 (enum dispatch で十分) の裏に置き、`src/mcp.rs` はそれを呼ぶだけにする。tool schema / 応答 JSON / approval metadata は byte-compatible に保つ (gateway contract snapshot が差分ゼロであること)。

- [ ] 4 backend の entry point を共通 dispatch に集約 (behavior change なし)
- [ ] 共通 task contract (start / get / control / list) と backend-specific options / capability 型を定義
- [ ] approval / receipt / reconcile / evidence ownership を core の共通入口へ移す (`src/mcp.rs` から承認処理を剥がす)
- [ ] execution / verification / delivery state と論理 task / execution の区別を record に導入
- [ ] `task_list` を core に追加 (backend ごとの store を維持し、共通 index から始める)
- [ ] current MCP tools の behavior を regression test / gateway contract snapshot で固定

### Phase V — VCS transaction / jj-first managed workspace (high priority)

詳細 contract: `issues/open/20260925-vcs-transaction-jj-first.md`。

目的は agent の `git add/commit` 規律に依存せず、task workspace の working state を Temote 管理下の recoverable VCS state として捕捉すること。

- [x] V0: Git snapshot broker と jj-first の設計比較、VCS abstraction / snapshot / observation / delivery boundary
- [x] V1: temporary fixture で jj feasibility prototype。**jj-first viable**。bare Git backend、複数 `jj workspace`、change_id、crash後 snapshot、Git read-only compatibility、delivery ref を実測済み
- [x] V2: backend-neutral VCS/workspace contract を `issues/open/20260925-v2-vcs-workspace-contract.md` に固定。F1 generic parts を保持し jj-first / Git compatibility を分離
- [ ] V3: typed Temote VCS adapter + Observation hook
- [ ] V4: GitHub delivery integration / stacked PR strategy

**V1 decision まで F2/F3 の git-worktree-specific implementation を開始しない。**
A / O / B と F1 の repository identity・freshness・no-local-main の generic contract は並行して進めてよい。

### Phase O — observation / context continuity (high priority)

詳細 contract: `issues/open/20260925-observation-context-memory-plane.md`。

- [x] O0: observation boundary / raw-vs-derived authority / worker / Context Resolver contract
- [ ] O1: common orchestration boundary の owner-only observation journal。instruction は canonical task/evidence への reference-first とし、secret-bearing structured field を複製しない
- [ ] O2: LLM worker なしの deterministic Context Resolver。過去 instruction + verified task/execution/verification state だけで head switch を成立させる
- [ ] O3: asynchronous Memory Worker。checkpoint / support refs / dedupe / supersession / retry idempotency
- [ ] O4: knowledge-aware Context Resolver。current facts / decisions / constraints / unresolved / failure pattern を provenance 付きで返す
- [ ] worker failure / stale projection を task failure に読み替えず、last processed observation revision を明示する

実装上の優先順位は O1/O2 > D/E。ただし A の共通 identity を飛ばして各 backend に個別 logger を追加しない。

### Phase B — local frontend

- [ ] `ControlRequest` に task start/list/get/control を追加 + protocol version
- [ ] `temote-mcp task ... --local` CLI (rename 前は現 binary 名で提供)
- [ ] `--operation-id` 明示 / 再利用と再照合 UX
- [ ] MCP と local が同じ core path を通る E2E
- [ ] 同じ権限の cloud / local caller が接続を切り替えても承認を繰り返さず、同一 task / operation / evidence を扱えることを検証
- [ ] agent mode の既存許可内の start / control が yolo なし・Temote 追加 prompt なしで動き、scope 外操作は拒否されることを検証

### Phase R — reconciliation acceptance

- [ ] local 開始 task を MCP client から同一 task として追跡・制御 (逆も)
- [ ] client 切断後の再発見、応答消失後の再試行で二重起動しない
- [ ] supervisor / backend 再起動後の状態照合。継続不能は明示報告し、不明を成功・失敗に読み替えない
- [ ] transport 間で approval / permission / scope / evidence 条件が同一

### Phase C0 — gh-git identity fix (independent)

- [ ] linked worktree で profile を `--git-common-dir` 基準に解決する fix + test

### Phase C — workspace assignment and write exclusivity

- [ ] session / workspace 対応の決定 (初期案: workspace ごとに session、active writer 1)
- [ ] session 停止時の workspace 扱い
- [ ] 管理操作 (Temote 固定 adapter) と コード操作 (agent) の区別・実行権限を明示し AGENTS.md を更新
- [ ] gh-git に `workspace` namespace と JSON contract を設計 (passthrough collision policy と整合)
- [ ] Phase F の新規 bare store / no-local-main 契約との integration、および既存 managed-worktree layout の保全
- [ ] owner 付き repo 指定 (`owner/repo`) と現行 `<src-root>/<repo>` identity の mapping 方針
- [ ] task -> workspace binding
- [ ] reservation / concurrent task ownership を維持
- [ ] legacy worktree を自動移動/削除しない

### Phase D — environment preparation

- [ ] workspace ready-state abstraction (Temote environment 層。gh-git には持たせない)
- [ ] vp / pnpm preparation adapter (pinned version で flag / store 挙動を確認)
- [ ] Cargo / sccache / isolated target adapter (child env 経由)
- [ ] cleanup policy
- [ ] cache hit / cold start の representative benchmark
- [ ] 許可済み workspace / network 範囲の preparation が agent mode で yolo・追加 prompt なしに完了することを検証

### Phase E — gh-stack delivery

- [ ] task graph から delivery 対象 branch と依存を明示選択する mapping (単独 PR を通常 delivery として扱う)
- [ ] 単独 PR / `gh stack link` integration と delivery operation class。許可済み commit / push / PR 作成は agent mode で再承認しない
- [ ] delivery 前の fetch / base revision 検査と、必要な追従後の再検証
- [ ] stack / PR state evidence (GitHub API 経由)
- [ ] agent tasks が sibling worktree を mutate しないことを確認
- [ ] worktree-sensitive gh-stack commands (`init/add/checkout/sync/rebase`) の採用範囲を明示

### Phase F — initial repository store / no-local-main foundation

- [ ] `ManagedRepository::primary_checkout` 前提を RepositoryStore + Workspace へ抽象化 (契約設計は Phase A と並行)
- [ ] 新規 managed store は bare-first を標準とし、local `main` の作成・維持を不要にする
- [ ] store 管理操作の実行権限・agent mode の既存許可継承を明示し、実装 packet で AGENTS.md / policy tests を更新
- [ ] `origin/main` を取得・更新する fetch refspec と task / stack 基点の記録
- [ ] fetch 失敗時の freshness 未確認、長期 task 再開 / delivery 前の追従・再検証契約
- [ ] 通常操作を通して `refs/heads/main` が不要なこと、task commits が専用 branch に残ることを検証
- [ ] normal checkout / existing repos migration contract (dirty / ahead / diverged な local main を破棄しない)
- [ ] recovery / GC / orphan behavior (未コミット変更・未提出 commit の保護)
- [ ] macOS / Linux acceptance

### Phase G — product rename

- [ ] `temote` command surface
- [ ] `temote-mcp` compatibility / migration plan (binary alias, env prefix, state dir, socket path)
- [ ] docs / package / release / plugin / skill references
- [ ] release acceptance

## Implementation guide for small-model execution

この節は child issue を具体化するための作業指示であり、umbrella を一括実装する指示ではない。モデルの価格帯や名称で完了基準を下げない。**1 回の実行には 1 packet だけを渡す**。Phase A / B 等の見出し全体をそのまま実装依頼にしない。

### 1. Child issue を着手可能にする条件

指示役は下記テンプレートを埋め、実装役が設計判断とコーディングを同時に抱えないようにする。既に決めた 3 つの最上位要件は毎回再検討しない。

1. 対象 repository、作業 branch、確認した HEAD、親 issue と packet ID を記載する。
2. 前提 packet の成果を commit / ファイル / test 名で指す。未実装の API を存在するものとして書かない。
3. 入力・出力・副作用・失敗時の結果を具体例で固定する。新しい永続形式は schema version と旧データの扱いも書く。
4. 読むファイル・検索する symbol・変更対象を列挙する。行番号はずれるため symbol を優先する。
5. 手順を順番に書き、正常系と失敗系の期待値を指定する。
6. 実行する test の実在する名前とコマンド、必要な host / backend / credentials を記載する。
7. 今回完了する条件と、次 packet に残す項目を分ける。
8. commit / push / PR 操作は当該依頼の許可を記載し、そのまま引き継ぐ。禁止や追加承認を勝手に足さない。

「適切に設計」「必要に応じて統合」「全 backend をよしなに対応」で済ませない。下表の将来 packet は分割案であり、未決の wire format や policy を含むものは、先行する契約 packet の成果を child に転記してから着手可能にする。

### 2. 全 packet 共通の実行手順

1. 現在の HEAD・branch・remote・staged / unstaged / untracked 変更と AGENTS.md を読む。既存変更を今回の差分と区別して保持する。過去に読んだ baseline を現在の実装と決めつけない。
2. 指定 symbol とその呼出元・test を読む。shell を使える環境では下の検索例を使う。GitHub 経由の場合も同じファイルと symbol を取得する。
3. 変更前の関連 test を実行する。既存失敗を記録し、今回の変更による失敗と分ける。環境不足なら未実行の対象と不足物を記録する。
4. 最小の変更を行う。既存処理の抽出では、判定順序、error、JSON、receipt 保存順序、子プロセスの所有者を変えない。
5. 指定した正常系・失敗系を検証する。test を通すために assertion / scope 検査 / approval 検査を削除しない。
6. diff を読み、目的と無関係な rename・format・依存更新・schema 変更が入っていないか確認する。必要な修正後に関連 test を再実行する。
7. repository の必要な gates を実行し、許可に従って commit / push する。最後に git status と保存された commit を確認する。
8. packet の完了条件が揃った場合だけ閉じる。実装済みでも host acceptance が未実行なら、その状態を明記する。次 packet を実施したことにはしない。

検索の入口 (確認済み baseline に存在するファイル / symbol。後続では前提 commit で再確認):

```sh
rg -n 'authorize_.*operation|task_start|task_control' src/mcp.rs
rg -n 'TaskRecord|TaskStore|OperationReceipt|task_start|task_get|task_control' src/codex_app_server.rs src/opencode_server.rs src/devin_acp.rs src/devin_cloud.rs
rg -n 'ControlRequest|run_supervisor' src/session_control.rs
rg -n 'enum Command' src/cli.rs
rg -n 'ManagedRepository|primary_checkout|reservation|admission' src/managed_worktree.rs
rg --files -g AGENTS.md -g '*test*' -g '*contract*' -g '*snapshot*'
```

Temote の Rust / protocol 変更では AGENTS.md の gates を使う:

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
git diff --check
```

gateway / shared protocol を変更する場合は gateway directory で `npm test` も実行する。sandbox 内では AGENTS.md の `just sandboxed-check` と host-only gates の区別を守る。gh-git はその repository の AGENTS.md と Go の検証手順を使う。文書だけの packet に runtime acceptance の PASS を付けない。モデルが実在しない test 名や未提供 credentials を補ってはいけない。

### 3. 分割順と各 packet の成果

以下の「前提」は着手に必要な成果を示す。設計 packet の成果は実装完了ではない。F1 は A 系と並行して設計でき、C0 も独立に実施できる。同時実行や別 agent の起動を必須にはしない。

| ID | 前提 | 今回の成果 | 完了を確認する方法 |
| --- | --- | --- | --- |
| A1 | 現状確認 | 4 backend の dispatch と承認経路を共通入口へ抽出 | 同じ入力の JSON / approval metadata / error が維持される |
| A2 | A1 | typed request と capability の境界 | backend 固有入力を保持し、未対応 action を副作用前に拒否 |
| A3 | A2 | session-owned task 一覧と共通参照 | backend store を横断しても他 session の task が混ざらない |
| A4 | A3 | execution / verification / delivery の別状態 | execution completed だけで verification PASS にならない |
| B0 | A2、A3 | local protocol・認証 scope・実行所有者の契約 | request / response / version / error / lifetime の例が揃う |
| B1 | B0 | supervisor socket の task request routing | local request が A 系の同じ共通入口へ到達する |
| B2 | B1 | task CLI と operation ID の保存・再利用 | 応答消失後の再送が同じ operation を参照する |
| R1 | B2、A4 | 切断・再送と transport 相互操作の検証 | local → MCP と MCP → local で同一 task を追跡 |
| R2 | R1 | 再起動後の reconciliation | 観測不能な状態を成功・失敗に変えず二重起動しない |
| C0 | gh-git 現状確認 | linked worktree の profile 解決修正 | common Git dir の同じ identity を参照 |
| F1 | 現状確認 | store / workspace JSON と ref / path / freshness 契約 | 下記の具体的な入出力例と不変条件が揃う |
| F2 | F1、C0 | gh-git の新規 bare store と inspect | local main なしで origin/main を fetch・参照できる |
| F3 | F2 | task branch / worktree の ensure と再試行 | 同じ要求で同じ workspace、競合要求で上書きしない |
| F4 | F3 | freshness と既存 checkout 保全・回収条件 | fetch 失敗を明示し、dirty / ahead / diverged を保持 |
| C1 | F4、A2 | Temote の workspace session binding | scope が割当 workspace と一致 |
| C2 | C1、R2 | writer 排他・停止・再起動時の所有権 | 同一 workspace の writer が二重に動かない |
| D0 | C2 | preparation の状態・許可・cache key 契約 | 未準備 / 準備中 / ready / 失敗を区別 |
| D1 | D0 | vp / pnpm adapter | lockfile を維持し、別 worktree の依存 view を壊さない |
| D2 | D0 | Cargo / sccache adapter | target を分離し、共有 cache がなくても結果を偽らない |
| E0 | C2、A4 | delivery 対象・許可・receipt の契約 | task と branch / PR の多対一を表現できる |
| E1 | E0 | 単独 PR の提出・照合 | 許可内は再承認せず、再試行で重複 PR を作らない |
| E2 | E1 | gh-stack link の adapter | 指定した順序・base と部分成功を remote で照合 |
| G1 | A–C / R / F の完了 | rename 互換性表と移行手順 | 旧名・旧 state の検出と新名の対応が明確 |
| G2 | G1 | 互換性を保つ rename と release acceptance | 旧 client / state を使う移行 test が通る |

### 4. 最初に渡す A1 の実装指示

**目的:** MCP の外からも同じ承認付き処理を呼べる入口を作る。local CLI、task_list、store 統合、workspace 作成はこの packet には含めない。

**読む場所:** AGENTS.md、`src/mcp.rs` の 4 backend の match arm と `authorize_*_operation`、4 backend module の entry point、`src/lib.rs`、既存 approval / gateway contract tests。

**実装方針:** 新規 core の入口は `src/orchestration.rs` (後続で必要なら directory module 化) とする。`src/delegation/` の旧 one-shot 実装は移動しない。既に同等の core が導入されている場合はそれを使い、重複 module を作らない。4 variant の enum dispatch を使い、動的 plugin registry や新しい外部依存は導入しない。

1. 現行の status / start / get / control ごとに、入力検証・承認・backend 呼出し・応答整形の順序をメモする。get が承認不要なら抽出後も維持する。
2. backend 呼出しと承認処理を共通入口へ移す。共通入口の error 型や session 型は既存型を再利用する。activity に必要な情報は引数等で渡し、core から MCP wire 型へ依存させない。
3. MCP 側は tool 名を backend / action に対応付け、共通入口を呼び、既存形式へ整形する。public schema、文字列、JSON shape、approval metadata を変更しない。
4. backend の TaskStore / receipt / fingerprint / runtime ownership は移動・再設計しない。既存の「受理を保存してから副作用」の順序をそのまま使う。
5. 既存 test で同じ入力を確認する。必要な追加 test は、agent mode で許可済み start が prompt なし、ask で既存承認経路を通る、拒否時は backend side effect が 0 回、の境界を検証する。
6. 全 4 backend の wiring と `network` feature 無効時の compile を確認する。1 backend だけ通った状態で A1 完了としない。
7. diff に schema / store / runtime ownership の変更が入っていないこと、MCP 側に承認と backend 呼出しの別経路が残っていないことを確認する。

**完了条件:**既存 MCP 契約を維持したまま共通入口を経由する。新しい動作がまだ利用者に見えなくても、この抽出だけで A1 は完了してよい。4 backend の実サービス接続は、必要な環境がある場合の acceptance と区別し、mock / fixture だけで実サービス PASS を報告しない。

### 5. A2–A4 の具体化

**A2: 入力の型と capability**

1. 現在の schema と validator から各 backend の必須 / 任意 field を表にする。
2. 共通 field と backend-specific options を分ける。既存 default と error の互換性を維持する。
3. hosted / host-local、resume 可否、停止の意味を独立した capability として扱う。Devin ACP の cloud option を local execution と決めつけない。
4. 不正入力、未対応 action、異なる session の task 操作では backend を呼ばない test を加える。

**A3: 一覧・共通参照**

1. backend store を source of truth とし、共通一覧は session scope 内の read-only projection から始める。最初から第二の authoritative task store を作らない。
2. backend / task ID / session instance の対応を保持する。現行 task ID を振り直さない。
3. 1 backend の取得失敗を空一覧と偽らず、部分結果と未確認 backend を区別する。paging / 件数上限は child の入出力例に固定する。
4. session A / B、各 backend、読取失敗を fixture で検証する。

**A4: 結果の状態**

1. execution の既存 status を保持し、verification / delivery は独立 field または関連 record に置く。
2. 検証がない旧 task は `not_run` とする。migration は実装前に旧 record の fixture と読取規則を固定する。
3. verification は検証対象 revision に束縛する。dirty workspace を検証した場合は commit SHA だけで同一内容と扱わず、検証対象 snapshot を識別するか未対応を明示する。
4. revision が変わった後に旧 PASS を現在の PASS と表示しないことを検証する。

### 6. B0–R2 の具体化

**B0 は契約だけを決める packet。** 以下を decision table と request / response の例にしてから B1 に渡す。

1. protocol version と対応しない version の error。既存 session lifecycle request の互換性。
2. session 指定は全 task 操作で明示する。`task get <id>` 等の略記も実際の CLI では `--session` または検証済みの接続 context が必要。ID から他 session を探索して自動選択しない。
3. CLI / MCP client の認証と session ownership、core に渡す permission context。
4. supervisor は routing / lifecycle を所有し、実処理は既存 session の実行境界を維持する。owner-only socket という理由だけで backend を非 sandbox の supervisor context から起動しない。
5. client 切断と session 停止を区別する。client 切断で task を取り消さず、session 停止は既存の停止契約に従って照合する。
6. ID 保存先、owner-only access、atomic write、保存失敗時に送信しない順序、再利用時の request fingerprint 検査。

**B1:** B0 の request を追加 → session / permission を解決 → A の共通入口へ渡す → bounded response を返す。最初は fixture backend で socket routing を確認する。認証・scope・version の拒否経路も同じ test suite に置く。

**B2:** CLI parsing → operation ID を送信前に保存 → request 送信 → task / operation ID と状態を表示。再送に新 ID を割り当てない。同一 ID の異なる payload は conflict。CLI が切断しても server を勝手に再起動しない。

**R1 / R2 の failure injection:** 各行を個別 test にし、注入位置と backend 起動回数を確認する。

| 注入位置 / 操作 | 期待結果 |
| --- | --- |
| 送信前に CLI が終了 | backend side effect なし。保存済み ID は再利用可能 |
| 受理後、応答だけ消失 | 再送で同じ task を得る。backend の重複起動なし |
| 同じ ID で異なる task 文面 | conflict。元の task は維持 |
| local 開始 → MCP get / control | 同じ権限なら同じ task に作用 |
| MCP 開始 → local get / control | 同上。追加の Temote 承認なし |
| 権限のない session から get / control | 拒否。他 session の evidence を返さない |
| client 切断 | task を継続し、再接続で発見 |
| supervisor / backend 再起動 | receipt と backend の観測結果から照合。根拠なく再起動・完了扱いしない |
| backend の状態取得失敗 | unknown / reconciliation_required 等で不確実性を保持 |

再起動時の自動継続が backend にない場合、継続可能と偽る必要はない。停止確認済み・照合待ち等を正確に返すことが acceptance である。

### 7. C0 / F1–F4 / C1–C2 の具体化

**C0 (gh-git repository):** primary と linked worktree の profile 解決を同じ common Git dir に揃える。変更対象は既存 profile path の解決とその tests。binding の保存形式変更や workspace command 追加を同時に行わない。

**F1 の契約書に必要な項目:**

- repository の識別子は host / owner / repo を区別する。同名 repo を directory basename だけで同一視しない。
- store path / workspace ID / canonical path / branch / base commit / last successful fetch 時刻 / freshness / schema version の具体的 JSON。
- ensure の同一要求再送、既存 branch の衝突、path escape、fetch 失敗の具体的 error。
- 新規標準と legacy checkout の判別、inspect が read-only であること、dirty / 未提出 commit の保全条件。
- Temote が所有権を判断し、gh-git が Git primitive を提供する境界。F1 で Git 側 API を固定し、C で同じ API を二重設計しない。

**F2 の手順:**

1. test 専用 temporary remote を作り、main に少なくとも 1 commit を用意する。実ユーザー repository を fixture に使わない。
2. 新規 bare store に origin と remote-tracking fetch を設定する。`refs/heads/main` の作成を必要としないことを検証する。
3. inspect が base commit / freshness を返すこと、remote main を進めた後の fetch で観測値が更新されることを確認する。
4. 外部 GitHub の認証 test は C0 / integration と分離する。local remote fixture で OAuth 成功を主張しない。

**F3:** 指定 base commit から task branch と worktree を作る。同じ workspace ID / 同じ要求は再利用し、同じ ID / 異なる branch・repo・base は既存内容を上書きせず conflict。作成途中で失敗した場合も、既存 user path を削除せず inspect で照合できる状態を残す。

**F4:** remote を取得不能にした test で freshness 未確認を確認する。legacy fixture は clean / dirty / ahead / diverged を用意し、inspect・失敗・cleanup 判定の前後で refs とファイル内容が保持されることを確認する。自動で force reset / force remove する修正は不可。

**C1:** F の workspace 結果を canonicalize → 既存 reservation を取得 → その scope の session に bind → task を開始する。途中失敗で予約が漏れないこと、scope が親 repo 全体へ広がらないことを検証する。

**C2:** 同一 workspace への並行 start では writer が 1 つ、別 workspace なら独立して動くことを検証する。session 停止後の process 生存が未確認なら writer reservation を安全と決めつけて解放しない。停止確認・再起動後の照合・未コミット保全を個別 test にする。

### 8. D / E / G の具体化

**D0:** readiness の key に workspace・lockfile・toolchain・adapter version 等の必要な入力を含める。変更後に古い ready を使い回さない。adapter の command / env は固定の typed input から作り、caller の raw argv を受けない。install script 等が実行される可能性を含め、既存許可内の sandbox / network 条件を維持する。

**D1:** pinned vp / pnpm の実際の help / documentation で対応 flag を確認してから固定 command を実装する。本書の例を未検証のままコピーしない。正常 install、lockfile 不一致、network 失敗、2 worktree の依存差を検証する。cache reuse は測定結果として記録し、install 自体が常に不要になると保証しない。

**D2:** shared CARGO_HOME / sccache と workspace-specific target を child env に設定する。sccache がない・失敗した場合の fallback または error は D0 の契約に従う。incremental を切る設定は測定対象とし、速度改善を未測定で PASS にしない。

**E0:** repo / branches / operation class に対する既存許可、対象 revision、単独 PR / stack の選択、remote side effect の receipt と再照合規則を固定する。task の完了だけで勝手に提出しない。提出を含む既存依頼は再承認しない。

**E1:** fetch・base 検査 → 指定 revision の検証結果確認 → 許可済み push → 既存 PR 照合 → 必要なら作成 → evidence 記録。応答消失時は remote state を照合してから再試行する。主な test は、許可内で追加 prompt なし、scope 外拒否、PR 作成直後の応答消失、検証後の revision 変更。

**E2:** 明示された branch 順で link する。途中まで push / PR 作成 / base 更新が成功した fixture から再照合できることを検証する。additive-only の制約を越える削除・並替えは実装しない。実 GitHub acceptance は当該 task の許可範囲の test repo / branches を使う。

**G1 / G2:** binary、env prefix、state / socket path、gateway、skill、release の旧→新対応を先に表にする。各互換項目を小さい変更に分割する。既存 state を見つけられず新規 task を重複起動する移行は不可。CalVer は既存 release workflow に従い、手作業で version を進めない。

### 9. Child issue テンプレート

次の項目を埋めて 1 packet を渡す。未決箇所がある場合は「設計 packet」として成果を契約書に限定する。実装役に umbrella 全体の設計判断を押し付けない。

```markdown
# <packet ID>: <今回完成する動作>

Status: ready | contract-needed
Repository:
Branch / observed HEAD:
Parent issue:
Prerequisites: <完了 commit / 対象ファイル / test>

## 1. Goal
<利用者または次 packet が新しくできることを1つ>

## 2. Fixed decisions
<入力・出力例、error、権限、永続化、互換性>
<3つの最上位要件に対する今回の責務>

## 3. Read / change scope
<実在するファイル・symbol・test>
<既存変更と今回の変更の扱い>

## 4. Steps
1. <現状確認>
2. <最小変更>
3. <正常系検証>
4. <失敗系検証>
5. <diff / gates / 保存確認>

## 5. Acceptance
- [ ] <入力→期待結果>
- [ ] <失敗条件→期待結果>
- [ ] <維持する互換性>

## 6. Validation commands
<実在するコマンドと必要環境。未実行のhost gateも明記>

## 7. Delivery authorization
<今回与えられた commit / push / PR 方針>

## 8. Completion report
<commit、変更、検証 PASS / FAIL / 未実行、残件、最終status>
```

### 10. 困った場合の判断と報告

1. symbol が移動した場合は repository 内で検索し、同じ責務の実装を使う。既存機能を再実装して埋めない。
2. 前提 packet が未完了なら、その不足を具体的に記録する。別 phase を抱き合わせて補完しない。
3. 失敗した test は原因を調べ、今回の範囲内なら修正・再テストする。目的を変える判断だけを指示役へ返す。命名や小さな内部関数分割は既存 style に従い自律的に決める。
4. 一時的な通信失敗では既存 operation / job の状態を先に確認し、重複起動しない。running を観測できる間は追跡を継続する。
5. 最終報告は「packet ID / commit」「新しく成立した動作」「実行コマンドと PASS・FAIL・未実行」「残件」「git status / remote 保存確認」。完了していない受入条件はチェックしない。

## Open questions

- A1 では新規 `src/orchestration.rs` を入口とし旧 `src/delegation/` は維持する。後続で directory module に分割する際の具体的な配置。
- 共通 index の置き場所と、backend store との整合を崩したときの照合規則。
- 既存許可を repo / branches / operation class に束縛して継承・失効させる record と、backend 固有 permission への写像 (許可済み範囲で再承認しない方針は確定)。
- fetch 失敗時の開始可否・長期 task の freshness 検査タイミングと、事前設定する policy の具体形。
- owner 付き repository 指定で owner 違い同名 repo をどう扱うか (現行 layout は repo 名のみ)。
- 1 session が複数 workspace を持つ別案を採る必要があるか (採るなら scope を広げない方法)。
- Devin Cloud task を stack delivery に参加させる場合、hosted session が作る branch をどう task graph に取り込むか。

## Acceptance criteria

- [ ] yolo なしの agent mode で、既存許可内の workspace 準備・実装・テスト・許可済み commit / push / PR 作成を Temote の操作ごとの再承認なしに進められる
- [ ] 同じ権限を持つ cloud / local caller 間で task の開始・追跡・制御・再試行を引き継げ、指示役の場所による追加承認や機能差がない
- [ ] 新規 managed repository は bare store + task worktrees が標準で、local main の checkout / commit / merge / pull を必要としない
- [ ] task 開始時と提出前に確認した origin/main の commit / 時刻を記録し、fetch 失敗を最新確認済みと扱わない
- [ ] 既存の dirty / ahead / diverged な checkout は保全し、新規 no-local-main 標準とは区別して移行状態を報告する
- [ ] MCP を通さず local client / CLI から同じ agent task lifecycle を操作できる
- [ ] MCP / local / HTTP / Gateway が同じ orchestration core を利用する
- [ ] Codex / OpenCode / Devin ACP が task ごとの isolated workspace で動作する
- [ ] coding agent が workspace path を勝手に決めない
- [ ] GitHub identity が repository-scoped で、linked worktree を含め global `gh auth switch` を必要としない
- [ ] concurrent task が sibling worktree / metadata / caches を破壊しない
- [ ] JS/TS worktree が vp + pnpm cache reuse で agent-ready になる
- [ ] Rust worktree が shared Cargo source/sccache + isolated target で agent-ready になる
- [ ] task graph から明示選択した delivery branch 依存を stacked PR (または単独 PR) として提出できる
- [ ] execution 完了・検証合格・提出完了が別状態として記録され、未検証を PASS と扱わない
- [ ] 応答消失・切断・再起動後も二重起動せず、状態不明を成功・失敗に読み替えない
- [ ] gh-stack integration が existing worktree ownership を壊さない
- [ ] workspace / task cleanup が uncommitted work を勝手に破棄しない
- [ ] agent が explicit commit を実行しなくても、managed workspace の変更を recoverable VCS state として Temote が捕捉できる
- [ ] task/execution と VCS before/after revision を相関し、head/backend 切替後も同じ logical work を引き継げる
- [ ] current server-backed delegation behavior (4 backend) の regression がない
- [ ] head を切り替えても、authorized scope 内で過去の instruction・verified task/execution state・relevant current knowledge を Context Resolver から取得できる
- [ ] coding agent に memory 保存・要約・knowledge 更新の追加 prompt/tool call を要求しない
- [ ] caller/agent の claim と Temote が evidence/state から確認した事実を区別し、worker failure / stale knowledge を task success/failure に読み替えない
- [ ] core/frontend separation 後、`temote` への rename migration が実行可能な状態になる

## Principle

Temote を「MCP server」ではなく、

> 指示役の cloud / local や使用する head に依存せず、yolo なしの agent mode で許可範囲内を極力再承認なしに実行し、local main の管理を必要としない workspace 基盤から、caller が明示した task と依存関係を coding agents で実行して、検証結果・evidence と PR / stacked PR まで運び、observable instruction / execution から次の head へ provenance 付き context を引き継げる development harness

として再定義する。
