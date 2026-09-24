# Temote を local / remote agentic development harness へ再構成する

Status: open / umbrella tracker  
Model: GPT-5.6 Sol  
Created: 2026-09-24 (Asia/Tokyo)

## Goal

`temote-mcp` を、MCP server を中心とした実装から **local / remote 両対応の agentic development harness / development orchestrator** へ段階的に再構成する。

最終的には product / binary の中心名を `temote-mcp` から **`temote`** へ寄せ、MCP は Temote の transport / frontend の一つとして扱う。

この issue は umbrella tracker とし、既存の server-backed agent delegation、managed worktree、gh-git integration を捨ててやり直すのではなく、現在の main の状態から責務を整理して移行する。

## Current direction

2026-09-24 時点で `local_agent_run` は削除済み。

コード操作は Temote 自身が shell / Git broker で代行するのではなく、server-mode の coding agent に委譲する。

- Codex: `codex app-server --stdio`
- OpenCode: `opencode serve`
- Devin: `devin acp` / `devin acp --cloud`

Related:

- `issues/open/20260922-agent-server-backends-cli-deprecation.md`
- `issues/done/20260916-managed-worktree-session-integration.md`
- `issues/done/20260916-agent-mode-git-broker-gh-git-integration.md`

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
        |                Devin                |
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

Temote 自身が各 Git command を emulation / proxy することは中心責務にしない。

## 1. Extract an orchestration core from MCP

現在 MCP tool 実装側に寄っている server-backed task orchestration を transport-independent core に引き上げる。

概念 API:

```text
Orchestrator
  task_start(...)
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

既存の公開 MCP tool 名は compatibility surface として維持可能。

内部では将来的に:

```text
task_start(backend = Codex)
task_start(backend = OpenCode)
task_start(backend = Devin)
```

へ normalize する。

## 2. Add local orchestration without MCP

ChatGPT Desktop や native local client から Temote を利用する場合、MCP round-trip を要求しない local path を追加する。

例:

```sh
temote task start --local \
  --backend opencode \
  --repo f4ah6o/example \
  --task implement-api

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
```

identity / workspace ownership / approval / credentials / evidence / activity policy は MCP / remote と共通にする。

### Local control plane

CLI process に agent server を所有させず、原則として既存 supervisor の owner-only local control socket を利用する。

```text
Desktop / CLI
    |
 local control protocol
    |
Temote supervisor
    |
    +-- task A -> Codex app-server
    +-- task B -> OpenCode serve
    +-- task C -> Devin ACP
```

Unix domain socket / Windows named pipe を transport とし、Temote-native RPC を versioned internal protocol とする。

将来的な利用先:

- Temote CLI
- ChatGPT Desktop-like local integration
- native GUI
- VS Code / JetBrains integration

## 3. Make gh-git the repository/workspace substrate

`f4ah6o/gh-git` を Temote 専用品にはせず、人間単独でも使える repository/workspace substrate として発展させる。

責務:

- repository-scoped GitHub identity / credential routing
- repository store
- worktree lifecycle
- workspace inspection
- machine-readable JSON contract
- agent-ready workspace preparation

候補 surface:

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

理想形では bare repository store + disposable worktrees を採用し、

```text
repository != workspace
```

を構造として明確化する。

ただし現在の Temote の managed worktree 実装は canonical primary checkout を前提にしているため、一気に置換しない。

移行順:

1. gh-git workspace と現行 Temote managed-worktree layout を整合
2. workspace creation / inspection primitive を gh-git へ寄せる
3. Temote の `PrimaryCheckout` 前提を `RepositoryStore + Workspace` abstraction へ置換
4. bare-first を opt-in
5. acceptance 後に default 化を検討

### Ownership boundary

- gh-git: Git 的な repository / worktree primitive
- Temote: task ownership / reservation / concurrent agent safety

他 task / 他 agent の worktree を勝手に remove / prune しない invariant は Temote 側に残す。

## 4. Agent-ready workspace preparation

worktree が Git 的に存在するだけでは ready としない。

```text
Git ready
+ dependencies ready
+ toolchain ready
+ cache ready
= agent-ready workspace
```

workspace provisioning に environment preparation を含める。

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

### Rust / Cargo

依存 source/index/git checkout は通常の shared `CARGO_HOME` を利用。

build artifact は全 worktree 共通 `target/` にしない。

推奨:

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

worktree cleanup 時に workspace-scoped target cache を回収可能にする。

## 5. Integrate official github/gh-stack for delivery

stacked branch / stacked PR 自体は Temote / gh-git で再実装しない。

official `github/gh-stack` を delivery backend として利用する。

初期 integration は、worktree-aware branch management と衝突しにくい `gh stack link` を中心にする。

例:

```text
Temote task graph

task-auth
  |
task-api
  |
task-ui

        -> branches

main
  |
feat/auth
  |
feat/api
  |
feat/ui

        ->

gh stack link feat/auth feat/api feat/ui
```

これにより:

```text
agent task graph ~= branch graph ~= PR stack
```

を実現する。

初期段階では gh-stack の `init/add/sync/rebase` に worktree lifecycle の authoritative ownership を持たせない。

Temote / gh-git:

- branch / worktree lifecycle

gh-stack:

- GitHub Stack
- PR base relation
- stacked PR create/link/status/merge

machine-readable stack state を evidence / task delivery state に取り込む。

## 6. Repository-scoped GitHub identity across worktrees

gh-git の repository-scoped identity を repository store / common Git configuration に保持し、すべての linked worktree と agent task で同じ GitHub account mapping を利用する。

```text
1 Git repository
= 1 GitHub identity
= N worktrees
= N Temote tasks
```

Codex / OpenCode / Devin server spawn 時に必要なら gh-git の tokenless `GH_CONFIG_DIR` profile を環境へ渡す。

以下を避ける。

- global `gh auth switch`
- remote owner 名から GitHub login を推測
- raw token を task/evidence/activity に出す

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

`temote-mcp` binary / package / command は compatibility migration を設計して段階的に `temote` へ移行する。

この issue では rename を即時実施せず、core/frontend separation が成立してから migration packet を切る。

## Proposed domain boundaries

内部構造は機能の寄せ集めではなく以下の domain を意識して整理する。

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
  devin/
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
- `--local` を unrestricted / yolo mode にすること
- rename のためだけに compatibility を壊すこと

## Suggested implementation packets

### Phase A — core extraction

- [ ] current server-backed task lifecycle を MCP adapter から orchestration core へ抽出
- [ ] Codex / OpenCode / Devin を共通 task contract の backend として整理
- [ ] current MCP tools の behavior を regression test で固定

### Phase B — local frontend

- [ ] supervisor local protocol に task start/list/get/control を追加
- [ ] `temote task ... --local` UX を追加
- [ ] MCP と local が同じ core path を通る E2E
- [ ] `--local` でも approval/security policy が変わらないことを検証

### Phase C — gh-git workspace

- [ ] f4ah6o/gh-git に workspace JSON contract を設計
- [ ] current managed-worktree layout との compatibility integration
- [ ] task -> workspace binding
- [ ] reservation / concurrent task ownership を維持
- [ ] legacy worktree を自動移動/削除しない

### Phase D — environment preparation

- [ ] workspace ready-state abstraction
- [ ] vp / pnpm preparation adapter
- [ ] Cargo / sccache / isolated target adapter
- [ ] cleanup policy
- [ ] cache hit / cold start の representative benchmark

### Phase E — gh-stack delivery

- [ ] task dependency -> branch dependency mapping
- [ ] `gh stack link` integration
- [ ] stack / PR state evidence
- [ ] agent tasks が sibling worktree を mutate しないことを確認
- [ ] worktree-sensitive gh-stack commands の採用範囲を明示

### Phase F — repository-store abstraction

- [ ] PrimaryCheckout 前提を RepositoryStore + Workspace へ抽象化
- [ ] bare-first opt-in
- [ ] normal checkout / existing repos migration contract
- [ ] recovery / GC / orphan behavior
- [ ] macOS / Linux acceptance

### Phase G — product rename

- [ ] `temote` command surface
- [ ] `temote-mcp` compatibility / migration plan
- [ ] docs / package / release / plugin references
- [ ] release acceptance

## Acceptance criteria

- [ ] MCP を通さず local client / CLI から同じ agent task lifecycle を操作できる
- [ ] MCP / local / HTTP / Gateway が同じ orchestration core を利用する
- [ ] Codex / OpenCode / Devin が task ごとの isolated workspace で動作する
- [ ] coding agent が workspace path を勝手に決めない
- [ ] GitHub identity が repository-scoped で、global `gh auth switch` を必要としない
- [ ] concurrent task が sibling worktree / metadata / caches を破壊しない
- [ ] JS/TS worktree が vp + pnpm cache reuse で agent-ready になる
- [ ] Rust worktree が shared Cargo source/sccache + isolated target で agent-ready になる
- [ ] task dependency graph から stacked PR を作成できる
- [ ] gh-stack integration が existing worktree ownership を壊さない
- [ ] workspace / task cleanup が uncommitted work を勝手に破棄しない
- [ ] current server-backed delegation behavior の regression がない
- [ ] core/frontend separation 後、`temote` への rename migration が実行可能な状態になる

## Principle

Temote を「MCP server」ではなく、

> isolated, prepared workspaces を coding agents に割り当て、task graph を実行し、evidence と stacked PR まで運ぶ local / remote development harness

として再定義する。
