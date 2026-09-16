# agent mode の Git UX を通常の `git` に戻し、内部 broker + `gh-git` で安全に処理する

Status: open
Model: GPT-5.6 Sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 developer workflow friction
Type: local agent / Git / gh-git / sandbox / developer UX
Related:
- `issues/done/20260916-structured-git-branch-worktree-operations.md`
- `issues/open/20260916-repo-scoped-github-account-selection.md`
- `issues/open/20260916-local-agent-process-spawn-eperm.md`
- `issues/open/20260916-readonly-git-inspection-safety-block.md`
- https://github.com/f4ah6o/gh-git

## Problem

Temote の `permission_mode=agent` は approval-free な structured operation を提供する一方、開発 agent から見た Git UX に大きな摩擦が残っている。

現在の `local_agent_run` sandbox は workspace を writable にしても `.git`, `.agents`, `.codex` を protected metadata として read-only にする。そのため Codex / OpenCode が通常の開発フローとして次を実行しても、Git metadata mutation が必要な操作は成立しない。

```text
git switch -c feat/example
git worktree add .wt/example -b feat/example
git commit -m "..."
git fetch
git push
```

この capability gap を埋めるため、`git_branch_create`, `git_switch`, `git_worktree_add` 等の bounded structured MCP operation は既に実装済みである。しかし Git の各操作を MCP tool として個別追加し続ける方式は、agent に Temote 固有 surface の知識を要求し、通常の developer workflow と乖離する。

今後 `merge`, `rebase`, `cherry-pick`, `tag`, `worktree remove`, `restore`, `reset --mixed`, `push tag` 等が必要になるたび MCP schema を増やす構造は、長期的な UX / maintenance cost の両面で望ましくない。

## Observed `gh-git` capability

`f4ah6o/gh-git` は現在、通常の Git command を `gh git <git args...>` で shell を介さず passthrough できる。

代表例:

```text
gh git status
gh git diff --stat
gh git add -- path/to/file
gh git commit -m "message"
gh git switch -c feature/example
gh git worktree list
gh git worktree add ...
gh git fetch --prune
gh git pull --ff-only
gh git push
```

また repository-local binding により、GitHub identity / Git HTTPS credential helper を global `gh auth switch` に依存せず選択できる。

Temote repository `/home/hirohito-fujita/src/local-mcp` でも現在 repo-local binding は `f4ah6o` に設定され、credential helper は `gh git credential --managed` を利用する構成になっている。

ただし `gh-git` は intentionally generic passthrough であり、そのまま host-side unrestricted execution capability として公開してはならない。Git は config / hooks / aliases / credential helpers / filters / external commands 等へ到達できるため、Temote 側で bounded capability に再分類する必要がある。

さらに単に `gh-git` binary を local-agent sandbox 内へ見せるだけでは解決しない。最終的には Git が同じ sandbox 内で実行されるため `.git/worktrees` 等の protected metadata write restriction を受ける。

## Goal

agent mode では Codex / OpenCode に Temote 固有の `git_*` MCP tool を意識させず、一般的な Git command をそのまま使える developer UX を提供する。

一方で、ordinary sandbox の protected metadata / filesystem containment / network / credential isolation は維持する。

目標アーキテクチャ:

```text
local_agent_run
  |
  +-- Codex / OpenCode sandbox
  |     |
  |     +-- git status
  |     +-- git add
  |     +-- git commit
  |     +-- git switch
  |     +-- git worktree add
  |     +-- git fetch / push
  |           |
  |           v
  |       private git shim
  |           |
  +-----------+--> Temote internal Git broker
                  |
                  +-- validated local Git execution
                  +-- structured Git metadata mutation
                  +-- host-side network operation
                  +-- gh-git repo-scoped credential routing
```

agent が見る command syntax は通常の Git のままにし、Temote 内部で operation を分類・検証・実行する。

## Proposed design

### 1. local-agent private Git shim

`local_agent_run` 専用環境の `PATH` 先頭に Temote-owned `git` shim を配置する。

Codex / OpenCode からは通常の `git` executable に見えるが、shim は arbitrary shell execution を行わず、request を Temote parent-side broker へ転送する。

shim は caller-controlled executable path / arbitrary host command / broad sandbox escape を提供しない。

### 2. Git command classifier

受け取った argv を Git operation class に分類する。

最初の候補:

```text
inspection
  status
  diff
  log
  show
  rev-parse
  branch --list
  tag --list
  worktree list
  remote -v

workspace/index mutation
  add
  restore
  commit

branch/worktree metadata mutation
  branch
  switch
  worktree add
  worktree remove

network
  fetch
  pull
  push
  ls-remote
```

raw passthrough ではなく、operation ごとに許可 grammar と safety invariant を持つ。

### 3. existing structured Git implementation の再利用

既に実装済みの `git_branch_create`, `git_switch`, `git_worktree_add`, `git_add`, `git_commit`, `git_fetch`, `git_pull`, `git_push` の validation / sandbox / host-side execution logic を broker backend として再利用する。

特に `git worktree add` は current structured implementation が持つ次の invariant を維持する。

- destination は repository-owned `<repository>/.wt/<safe-name>` のみ
- arbitrary external destination を受け付けない
- exact structured operation 中だけ common `.git/worktrees` parent に必要な write capability を与える
- existing sibling private metadata は read-only mask
- `--force` / reset / stash / arbitrary refspec / arbitrary URL を公開しない

MCP tool 自体は orchestration / compatibility surface として残してよいが、agent mode の通常フローでは直接要求しない。

### 4. GitHub network operation は `gh-git` binding を利用

GitHub HTTPS remote では、repo-local binding が存在する場合に `gh-git` の managed credential contract を backend として利用する。

目的:

- `gh auth switch` を自動実行しない
- ambient active account に依存しない
- repository ごとに GitHub identity を固定できる
- concurrent repository / concurrent agent が global account state を奪い合わない
- raw token を MCP output / log / argv / activity に出さない

`owner == GitHub login` は一般に成り立たないため、remote owner 名から auth identity を推測しない。repo-local `gh-git` binding を authoritative mapping とする。

### 5. direct `gh` integration は別 capability とする

`gh pr create`, `gh issue create`, `gh api` 等は Git command ではないため、この Git broker へ混ぜない。

必要な場合は `gh-git` が生成する tokenless `GH_CONFIG_DIR` profile / repository binding を利用した separate GitHub broker として扱う。

## MCP surface policy

既存 `git_*` MCP tools は即時削除しない。

位置付けを次のように整理する。

```text
MCP git_* tools
  = orchestration / compatibility / non-agent client surface

agent mode
  = normal git syntax
    -> private Git shim
    -> Temote internal Git broker
```

これにより、今後 Git command ごとに public MCP schema を増やすことを原則不要にする。

## Security constraints

- ordinary `execute` / `start_command` の `.git` read-only contract は維持する。
- local agent に Git metadata root 全体の broad write permission を与えない。
- `gh-git` generic passthrough を host-side unrestricted command としてそのまま公開しない。
- Git alias / `-c` arbitrary config / hooksPath / external diff / filter / credential helper injection 等、external command execution に繋がる grammar は fail-closed にする。
- raw remote URL / arbitrary refspec / force push / force checkout / hard reset は initial surface に含めない。
- repository owner から GitHub account identity を推測しない。
- `gh auth switch/login/logout` の global mutation を自動実行しない。
- token / Authorization header / keyring material を output / logs / activity / issue evidence に出さない。
- dirty / untracked work を reset / checkout-force / stash / delete しない。
- existing sibling worktree metadata を mutation しない。

## Initial implementation slice

最初の slice は representative developer workflow に限定する。

```text
git status
git diff
git log
git add
git commit
git branch
git switch
git worktree list
git worktree add
git worktree remove
git fetch
git pull --ff-only
git push
```

`merge`, `rebase`, `cherry-pick`, `tag mutation`, `reset`, `restore --staged` 等は classifier / safety contract が固まった後に追加評価する。

## Acceptance criteria

- [ ] `permission_mode=agent` の `local_agent_run` で Codex / OpenCode が Temote 固有 `git_*` tool を知らなくても通常の `git` command を使える。
- [ ] `git status`, `git diff`, `git log` が ordinary developer UX として動く。
- [ ] `git add` -> `git commit` が selected workspace/index scope だけを mutation する。
- [ ] `git switch -c <branch>` 相当を normal Git syntax から実行できる。
- [ ] `git worktree add .wt/test -b test` 相当が `.git/worktrees` broad write permission なしで成功する。
- [ ] worktree add/remove が existing sibling metadata と他作業者 worktree を変更しない。
- [ ] GitHub HTTPS repository で `git fetch/pull/push` が repo-local `gh-git` identity を使用し、global active `gh` account に依存しない。
- [ ] operation 前後で `gh auth status` の global active account が変化しない。
- [ ] repository binding missing / credential unavailable / permission denied を区別して fail-closed にする。
- [ ] force push / arbitrary URL / arbitrary refspec / arbitrary config injection / external command escape を拒否する。
- [ ] protected metadata / secret isolation / workspace containment の既存 regression tests が green。
- [ ] Linux / macOS の local-agent Git broker regression coverage を追加する。
- [ ] representative E2E として agent が `worktree add -> edit -> add -> commit -> push` を Temote 固有 Git tool 指示なしで完走する。

## Test ideas

- normal clone: status -> branch create -> switch -> edit -> add -> commit
- dirty current worktree: isolated worktree add 後も元変更が保持され、新規 commit に混入しない
- linked worktree: sibling metadata write denial
- concurrent repo A/B: different `gh-git` bindings で fetch/push して identity が交差しない
- malicious Git argv: `-c core.hooksPath=...`, alias injection, `--config-env`, external diff/filter/helper override を reject
- path/ref PBT: branch/worktree name, `..`, absolute path, option-like input, revision expression escape
- network regression: raw local-agent process から arbitrary outbound Git URL へ接続できない
- compatibility: existing public `git_*` MCP tools の behavior は維持

## Non-goals

- local agent に unrestricted host shell を与えること
- Git の全 subcommand / 全 option を初期実装でサポートすること
- `gh-git` 自体を Temote sandbox security boundary として扱うこと
- global `gh` account state を repository ごとに切り替えること
- public MCP endpoint から yolo 相当の Git capability を提供すること
