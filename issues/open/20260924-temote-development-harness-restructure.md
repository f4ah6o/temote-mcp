# Temote を local / remote agentic development harness へ再構成する

Status: open / umbrella tracker (polished; implementation not started)
Model: GPT-5.6 Sol
Created: 2026-09-24 (Asia/Tokyo)
Updated: 2026-09-24 (Asia/Tokyo) — current `main` baseline / gh-git / gh-stack facts verified; scope / responsibility boundaries revised per PR #47 design review
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

**AGENTS.md からの境界変更**: 現行 AGENTS.md は「machine operations は agent に委譲する」としている。本 issue は workspace / environment / delivery の **管理操作** を Temote の固定 adapter が担い、実装中の **コード操作** は引き続き agent が担う、という区別を導入する。各 child issue (Phase C / D / E) は、どの操作がどちらに属するか、および固定 adapter に適用する実行権限 (sandbox profile / network / approval class) を明示し、AGENTS.md を同じ packet で更新する。

## Target architecture

```text
                         Temote
                           |
                    Orchestration Core
                           |
        +------------------+------------------+
        |                  |                  |
     workspace           agent             delivery
        |                  |                  |
      gh-git        Codex / OpenCode /      gh-stack
        |          Devin ACP / Devin Cloud    |
        |                  |                  |
   vp / Cargo cache      code             stacked PR
        |
        +------------ Development Harness ------------+

frontends / transports:
  - Local CLI / Desktop
  - MCP
  - HTTP
  - Gateway
```

Temote の中心責務:

- repository / workspace の選択
- task / dependency graph
- agent backend の選択と server lifecycle
- workspace reservation / ownership
- approval / evidence / activity
- delivery / stacked PR orchestration

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
  workspace_ensure(...)
  stack_submit(...)
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
  --task "implement api"

temote task list --local
temote task get --local <task-id>
temote task steer --local <task-id> "testsも追加して"
temote task stop --local <task-id>
```

`--local` の意味は **transport が local であることだけ**。

```text
--local != yolo
--local != unrestricted filesystem
--local != approval bypass
--local != sessionless
```

identity / workspace ownership / approval / credentials / evidence / activity policy は MCP / remote と共通にする。task は MCP と同じく session に束縛する。

### Retry / reconciliation UX

`operation_id` の CLI 自動生成だけでは、応答消失後にコマンドを再実行したとき別 ID で二重起動しうる。Phase B の契約に以下を含める (具体形は child で確定):

- caller が `--operation-id <uuid>` を明示でき、同一 ID の再送は同一 receipt を返す
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

## 3. Make gh-git the repository/workspace substrate

`f4ah6o/gh-git` を Temote 専用品にはせず、人間単独でも使える repository/workspace substrate として発展させる。

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

### Worktree-only / bare-first direction

最終的には repository と editable workspace を分離する。

```text
repository store
    |
    +-- worktree A -> Temote task A
    +-- worktree B -> Temote task B
    +-- worktree C -> Temote task C
```

理想形では bare repository store + disposable worktrees を採用し、`repository != workspace` を構造として明確化する。

ただし現在の Temote managed worktree は canonical primary checkout (`<src-root>/<repo>`) と `<src-root>/worktrees/<repo>/<name>` を前提にしているため、一気に置換しない。

移行順:

1. gh-git workspace layout を現行 Temote managed-worktree layout (`<src-root>/worktrees/<repo>/<name>`) と一致させる
2. workspace creation / inspection primitive を gh-git へ寄せる (Temote は JSON contract を消費)
3. Temote の `ManagedRepository::primary_checkout` 前提を `RepositoryStore + Workspace` abstraction へ置換
4. bare-first を opt-in
5. acceptance 後に default 化を検討

### Session / workspace mapping (Phase C の前提)

現状 task は session の canonical scope を workspace として使う。「task ごとの isolated workspace」を成立させるには、session と workspace の対応、同時 writer 数、session 停止時の扱いを先に決める。

初期案: **workspace ごとに session を持ち、同一 workspace の active writer を一つに制限する**。既存の path-scoped session / reservation と整合し、親 session の scope を広げずに済む。別案 (1 session が複数 workspace を持つ等) を選ぶ場合も、親 session の scope を暗黙に広げない方法を child issue に明記する。session 停止時に workspace を回収するか保持するかも同じ child で決める (uncommitted work は破棄しない)。

### Ownership boundary

- gh-git: Git 的な repository / worktree primitive
- Temote: task ownership / reservation / concurrent agent safety

他 task / 他 agent の worktree を勝手に remove / prune しない invariant、および Legacy worktree を adopt / move / delete しない invariant は Temote 側に残す。gh-git の `remove` / `prune` を Temote から呼ぶ場合も、Temote reservation を取得してからにする。

## 4. Agent-ready workspace preparation

worktree が Git 的に存在するだけでは ready としない。

```text
Git ready
+ dependencies ready
+ toolchain ready
+ cache ready
= agent-ready workspace
```

environment preparation は Temote environment 層の責務とし、gh-git の Git 側準備の後段に置く。preparation の実行は coding agent への委譲ではなく Temote / gh-git の固定 adapter が行う (caller は argv を指定できない。AGENTS.md の typed contract invariant と同じ)。network を要する install は既存 approval class / network policy に従う。

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
CARGO_TARGET_DIR=<Temote/gh-git managed cache>/<repo>/<workspace>
```

worktree cleanup 時に workspace-scoped target cache を回収可能にする。これらの env は backend spawn 時の child env (`src/child_env.rs`) 経由で渡し、caller 指定の env block は引き続き受けない。

## 5. Integrate official github/gh-stack for delivery

stacked branch / stacked PR 自体は Temote / gh-git で再実装しない。official `github/gh-stack` を delivery backend として利用する。

初期 integration は local tracking を持たない `gh stack link` を中心にする。

```text
Temote task graph        branches          GitHub

task-auth                main
  |                        |
task-api        ->      feat/auth    ->  gh stack link feat/auth feat/api feat/ui
  |                        |
task-ui                 feat/api
                           |
                        feat/ui
```

`task graph ~= branch graph ~= PR stack` は一般には成り立たない: 調査 task は branch を持たないことがあり、実装とレビューが同じ PR に関わることも、複数 task を一つの PR にまとめることもある。したがって task graph から **delivery 対象の branch とその依存関係を明示的に選び**、それを stack へ写像する。単独 PR も通常の delivery として扱い、stack は選択された branch 依存が 2 段以上ある場合の一形態とする。

gh-stack 統合と bare-first opt-in はいずれも共通コア / local frontend の成立条件にしない。

注意点 (verified):

- `link` は push / PR 作成 / base 修正 / Stack 作成を行う remote side effect であり、新しい approval class (delivery) として扱う。`ask` では approval、`agent` での扱いは Phase E child で決める。
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
task
agent
delivery
transport
```

例:

```text
orchestration/
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

## Suggested implementation packets

優先する到達点 (この順):

1. 共通コア抽出と既存 MCP 契約の維持 (Phase A)
2. local から同一 task lifecycle を操作 (Phase B)
3. 切断・応答消失・再起動・再試行の状態照合を検証 (Phase R)
4. workspace 割当と書込み排他 (Phase C)
5. environment preparation (D)、stack delivery (E)、bare-first (F) を個別に追加

依存関係: A → B → R → C → {D, E, F}。D / E / F は互いに独立で、いずれも A / B の成立条件ではない。C0 (gh-git identity fix) は独立した前提修正としていつでも進められる。G (rename) は A + B + R + C 完了後。

### Phase A — core extraction

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
- [ ] `--local` でも approval / permission mode / evidence policy が変わらないことを検証

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
- [ ] current managed-worktree layout との compatibility integration
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

### Phase E — gh-stack delivery

- [ ] task graph から delivery 対象 branch と依存を明示選択する mapping (単独 PR を通常 delivery として扱う)
- [ ] `gh stack link` integration + delivery approval class
- [ ] stack / PR state evidence (GitHub API 経由)
- [ ] agent tasks が sibling worktree を mutate しないことを確認
- [ ] worktree-sensitive gh-stack commands (`init/add/checkout/sync/rebase`) の採用範囲を明示

### Phase F — repository-store abstraction

- [ ] `ManagedRepository::primary_checkout` 前提を RepositoryStore + Workspace へ抽象化
- [ ] bare-first opt-in
- [ ] normal checkout / existing repos migration contract
- [ ] recovery / GC / orphan behavior
- [ ] macOS / Linux acceptance

### Phase G — product rename

- [ ] `temote` command surface
- [ ] `temote-mcp` compatibility / migration plan (binary alias, env prefix, state dir, socket path)
- [ ] docs / package / release / plugin / skill references
- [ ] release acceptance

## Open questions

- orchestration core を `src/delegation/` に置くか、新 module にして旧 one-shot delegate を別名に退避するか。
- 共通 index の置き場所と、backend store との整合を崩したときの照合規則。
- `agent` permission mode で `gh stack link` (remote push / PR 作成) を approval-free にしてよいか。
- owner 付き repository 指定で owner 違い同名 repo をどう扱うか (現行 layout は repo 名のみ)。
- 1 session が複数 workspace を持つ別案を採る必要があるか (採るなら scope を広げない方法)。
- Devin Cloud task を stack delivery に参加させる場合、hosted session が作る branch をどう task graph に取り込むか。

## Acceptance criteria

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

> どの coding agent に任せても適切な作業場所・権限・状態追跡・成果確認を提供し、isolated, prepared workspaces を coding agents に割り当て、caller が明示した task と依存関係を実行し、検証結果・evidence と PR / stacked PR まで運ぶ local / remote development harness

として再定義する。
