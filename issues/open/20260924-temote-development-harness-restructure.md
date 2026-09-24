# Temote を local / remote agentic development harness へ再構成する

Status: open / umbrella tracker (polished; implementation not started)
Model: GPT-5.6 Sol
Created: 2026-09-24 (Asia/Tokyo)
Updated: 2026-09-24 (Asia/Tokyo) — current `main` baseline / gh-git / gh-stack facts verified; scope / responsibility boundaries revised per PR #47 design review; user priorities: approve-free agent mode, caller-location independence, no local main
Baseline inspected: `ba4c51c` (`main`, after PR #46 delegation-only tool surface)
Roadmap: `issues/ROADMAP-20260916-agent-mode-main-only.md`
Related:

- `issues/open/20260922-agent-server-backends-cli-deprecation.md` (server-primary backend umbrella)
- `issues/open/20260923-devin-acp-backend.md`
- `issues/open/20260924-devin-cloud-backend.md`
- `issues/done/20260916-managed-worktree-session-integration.md`
- `issues/done/20260916-agent-mode-git-broker-gh-git-integration.md`
- `f4ah6o/gh-git` (repository-scoped GitHub identity extension)
- `github/gh-stack` (official stacked PR extension)

## Goal

`temote-mcp` を、MCP server を中心とした実装から **local / remote 両対応の agentic development harness / development orchestrator** へ段階的に再構成する。

最終的には product / binary の中心名を `temote-mcp` から **`temote`** へ寄せ、MCP は Temote の transport / frontend の一つとして扱う。

この issue は umbrella tracker であり、直接実装しない。既存の server-backed agent delegation、managed worktree、gh-git integration を捨ててやり直すのではなく、現在の `main` から責務を整理して移行する。各 Phase は Flash-sized の child issue (`issues/open/` または `issues/polished/`) に切り出してから着手する。

## Top-level requirements (user-confirmed)

以下の 3 要件を設計・実装順序・受入判定の最上位に置く。ここで定めるのは target contract であり、下記 current baseline の実装済み事実とは区別する。

1. **yolo を使わず、極力 approve-free な agent mode。** repository / workspace / network / 操作範囲について既に与えられた許可を task に引き継ぎ、その範囲内の準備・実装・テスト・許可済み commit / push / PR 作成では操作ごとの再承認を要求しない。delivery という分類だけで毎回承認にしない。権限拡張や未許可の破壊的操作は明示的に扱い、sandbox・秘密情報保護・他作業の保護を維持する。
2. **指示役が cloud / local のどちらでも同じように使える。** authenticated caller が同じ権限を持つ場合、transport によって task の操作能力・承認方針・状態参照が変わらない。cloud から開始した task を local から追跡・制御でき、その逆もできる。指示役の場所と agent の実行場所 (host-local / hosted) は別の軸として扱う。
3. **独立して遅れ・未統合 commit の蓄積・分岐が生じる local main を持たない。** 新規 managed repository は bare repository store + task worktrees を標準とし、local `main` を作業・統合・追従のために維持しない。基準は fetch で確認した `origin/main` とその commit。既存 checkout は勝手に移動・削除・reset せず、明示的な移行契約で保全する。

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
3. 切断・応答消失・再起動・再試行の状態照合を検証 (Phase R)。
4. bare-first / no-local-main の新規 store (Phase F) と workspace 割当・書込み排他 (Phase C) を初期基盤として完成させる。
5. environment preparation (D)、delivery (E) を個別に追加し、各操作の既存許可を引き継ぐ agent mode を検証する。

依存関係: A → B → R。C0 (gh-git identity fix) と F の設計は独立に開始できる。F の新規 store 実装は C0 と整合させ、C 完了には R + F + C0 を必要とする。C → {D, E}。G (rename) は A + B + R + F + C 完了後。F は後回しの opt-in ではない。各 phase は複数の小さい child packet に分け、設計・契約の決定と実装完了を区別する。

### Phase A — core extraction

最初の child は dispatch / 承認経路の抽出と既存契約の維持に限定し、record / index 拡張は後続 child に分ける。

最初の packet (推奨): 4 backend の `status` / `task_start` / `task_get` / `task_control` を 1 つの `Backend` 抽象 (enum dispatch で十分) の裏に置き、`src/mcp.rs` はそれを呼ぶだけにする。tool schema / 応答 JSON / approval metadata は byte-compatible に保つ (gateway contract snapshot が差分ゼロであること)。

- [ ] 4 backend の entry point を共通 dispatch に集約 (behavior change なし)
- [ ] 共通 task contract (start / get / control / list) と backend-specific options / capability 型を定義
- [ ] approval / receipt / reconcile / evidence ownership を core の共通入口へ移す (`src/mcp.rs` から承認処理を剥がす)
- [ ] execution / verification / delivery state と論理 task / execution の区別を record に導入
- [ ] `task_list` を core に追加 (backend ごとの store を維持し、共通 index から始める)
- [ ] current MCP tools の behavior を regression test / gateway contract snapshot で固定

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

## Open questions

- orchestration core を `src/delegation/` に置くか、新 module にして旧 one-shot delegate を別名に退避するか。
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
- [ ] current server-backed delegation behavior (4 backend) の regression がない
- [ ] core/frontend separation 後、`temote` への rename migration が実行可能な状態になる

## Principle

Temote を「MCP server」ではなく、

> 指示役の cloud / local に依存せず、yolo なしの agent mode で許可範囲内を極力再承認なしに実行し、local main の管理を必要としない workspace 基盤から、caller が明示した task と依存関係を coding agents で実行して、検証結果・evidence と PR / stacked PR まで運ぶ development harness

として再定義する。
