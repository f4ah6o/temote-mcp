# Proposal: Temote-managed Git worktree broker

## Status

open / umbrella tracker

Model: opencode-go/deepseek-v4.1-flash
Created: 2026-09-16
Updated: 2026-09-16
Roadmap: `issues/ROADMAP-20260916-agent-mode-main-only.md`

## Summary

Temote に Git worktree のライフサイクル管理を持たせ、AI coding agent が worktree の配置場所を任意に決定・作成しない構成にする。

worktree の標準配置先は以下に固定する。

```text
Canonical repository:
~/src/<repo>

Managed worktree:
~/src/worktrees/<repo>/<task>
```

例:

```text
~/src/
├── temote-mcp/
├── billone/
├── business-process/
└── worktrees/
    ├── temote-mcp/
    │   ├── issue-123-worktree-broker/
    │   └── feat-developer-execution-broker/
    ├── billone/
    │   └── app1274-detail-sort/
    └── business-process/
        └── flowable-migration/
```

このルールは `AGENTS.md` 等の repository-local instruction には依存させず、Temote broker 側で実際に強制する。

---

## Background

現在、AI coding agent が Git worktree を利用する際に以下の問題がある。

1. `../repo-branch` のような配置により `~/src` 直下が散らかる。
2. worktree の配置場所が agent / prompt / repository ごとに揺れる。
3. Temote sandbox / session root / filesystem permission と `.git/worktrees/*` の関係で摩擦が発生する。
4. agent が直接 `git worktree add` を実行すると、Temote が workspace lifecycle を管理できない。
5. `AGENTS.md` 等で規約を各 repository に配布する方式は、repository 数の増加に伴い保守・同期が困難になる。
6. AI model の prompt compliance に worktree policy の遵守を依存させたくない。

worktree の配置はコード規約ではなく、開発実行基盤の workspace policy として扱うべきである。

---

## Goals

### 1. worktree 配置を統一する

新規 worktree は必ず以下に作成する。

```text
~/src/worktrees/<repo>/<task>
```

### 2. AI coding agent に配置先を決めさせない

Codex / OpenCode 等の coding agent は、Temote から渡された project root 内で実装するだけとする。

agent 自身による `git worktree add` を通常フローから除外する。

### 3. Temote が workspace lifecycle を所有する

以下を Temote broker の責務とする。

- worktree discovery
- worktree creation
- worktree removal
- worktree prune
- session と worktree の関連付け
- worktree path policy の検証

### 4. repository-local policy を不要にする

以下のようなファイルへの worktree policy 複製を必須にしない。

```text
AGENTS.md
CLAUDE.md
README.md
```

policy の正本は Temote とする。

### 5. 既存 worktree を破壊しない

新 policy 導入時に既存の legacy worktree を自動移動・削除しない。

---

## Non-goals

本 proposal では以下を対象外とする。

- 通常の `git add`
- `git commit`
- `git diff`
- `git status`
- `git fetch`
- `git merge`
- `git rebase`
- Git コマンド全般の broker 化
- 既存 worktree の自動 migration
- repository 内の coding-agent instruction file の標準化

Git 全操作を MCP tool 化することは目的ではない。

broker 化の対象は、主として workspace / filesystem lifecycle に関係する操作とする。

---

## Directory policy

### Canonical repository

```text
~/src/<repo>
```

例:

```text
~/src/temote-mcp
~/src/billone
~/src/business-process
```

### Managed worktree

```text
~/src/worktrees/<repo>/<task>
```

例:

```text
~/src/worktrees/temote-mcp/issue-123-worktree-broker
~/src/worktrees/billone/app1274-detail-sort
```

### Reserved root

```text
~/src/worktrees
```

は Temote-managed workspace namespace として扱う。

通常 repository の clone 先として利用しない。

---

## Naming policy

Git branch 名と worktree directory 名は分離する。

例えば、

```text
branch:
feat/20260916-worktree-broker

directory:
feat-20260916-worktree-broker
```

branch 名に `/` が含まれていても、そのまま directory hierarchy として解釈しない。

可能であれば Issue / task identifier を優先して、人間が識別しやすい名前にする。

例:

```text
issue-123-worktree-broker
app1274-detail-sort
flowable-migration
```

directory name は filesystem-safe な値へ正規化する。

ただし sanitize のみを security boundary として使用しない。

最終的な canonical path が必ず以下の配下であることを検証する。

```text
~/src/worktrees/<repo>/
```

---

## Proposed broker interface

最低限、以下の operation を Temote broker に持たせる。

```text
worktree_create
worktree_list
worktree_remove
worktree_prune
```

### worktree_create

概念的な interface:

```text
worktree_create(
  repository: "temote-mcp",
  branch: "feat/20260916-worktree-broker",
  task_name?: "issue-123-worktree-broker"
)
```

Temote が repository と task name から配置先を決定する。

結果:

```text
~/src/worktrees/temote-mcp/issue-123-worktree-broker
```

### Important

caller から任意の absolute path を指定できる interface にはしない。

以下のような API は採用しない。

```text
worktree_create(
  path: "/tmp/foo"
)
```

または、

```text
worktree_create(
  path: "../foo"
)
```

worktree location は broker policy により決定される。

---

## Repository resolution

`repository` は可能であれば単なる basename ではなく、Temote が既に把握している repository identity / session configuration / remote URL から解決する。

例えば以下を区別できることが望ましい。

```text
github.com/f4ah6o/temote-mcp
github.com/example/temote-mcp
```

canonical checkout の実 path を取得した上で、

```text
canonical:
~/src/temote-mcp

worktree namespace:
~/src/worktrees/temote-mcp
```

を導出する。

同名 repository collision が実際に発生する場合は、namespace 拡張を別途検討する。

---

## Path validation

`worktree_create` は少なくとも以下を検証する。

1. repository が既知の canonical repository である。
2. resolved worktree root が `~/src/worktrees/<repo>` である。
3. final path を canonicalize した結果も worktree root 配下である。
4. `..` 等による path traversal が発生していない。
5. symlink 等によって管理 root 外へ escape していない。
6. target directory が既存の別 worktree / unrelated directory と衝突していない。

policy 違反時は fail closed とする。

---

## Agent-mode integration

推奨フロー:

```text
Task requested
    ↓
Temote resolves canonical repository
    ↓
Temote checks existing worktrees
    ↓
Reuse appropriate managed worktree
or
Temote broker creates managed worktree
    ↓
Temote session root is set to managed worktree
    ↓
local_agent_run
    ↓
Codex / OpenCode performs implementation
```

coding agent は worktree の作成場所を選択しない。

coding agent の責務は、

```text
implementation inside supplied project root
```

に限定する。

---

## Agent enforcement

worktree policy を prompt compliance のみに依存させない。

Temote が coding agent を起動する前に project root を検証する。

### Allowed canonical checkout

```text
~/src/<repo>
```

### Allowed managed worktree

```text
~/src/worktrees/<repo>/*
```

worktree として検出された directory が managed root 外にある場合、新規 task の標準 workspace としては使用しない。

ただし legacy worktree を発見しただけで削除・移動してはならない。

---

## Existing legacy worktrees

例えば以下が既に存在する場合:

```text
~/src/billone-main
~/src/billone-fix-123
~/src/.wt/foo
/tmp/repo-worktree
```

policy 導入によって自動削除・自動移動しない。

扱いは以下とする。

```text
Existing:
preserve

New creation:
deny outside managed root

Future tasks:
prefer/create managed worktree
```

legacy worktree の cleanup は明示的な別操作とする。

---

## Session lifecycle

Temote session と managed worktree を明示的に対応付ける。

概念的には以下の metadata を保持できるとよい。

```text
session:
  name: issue-123
  repository: temote-mcp
  canonical_root: ~/src/temote-mcp
  workspace_root: ~/src/worktrees/temote-mcp/issue-123
  branch: feat/issue-123
  workspace_type: git-worktree
```

session が coding agent を起動する場合、

```text
cwd = workspace_root
```

とする。

---

## Removal safety

`worktree_remove` は destructive operation なので、以下を確認する。

- target が managed worktree であること
- target が expected repository に属すること
- dirty state
- untracked files
- active Temote session / running job の有無
- 他の session が同じ worktree を使用していないこと

dirty worktree を force remove する挙動を default にしない。

既存の未コミット変更を自動 stash / reset / checkout / delete しない。

---

## Prune behavior

`worktree_prune` は Git metadata の stale entry cleanup に限定する。

filesystem 上に実 worktree が存在する場合に、その directory を独自判断で削除しない。

---

## Error reporting

policy 違反時は、agent の Git エラーとして露出させるのではなく Temote 側で明示する。

例:

```text
Worktree path violates Temote workspace policy.

Repository:
  temote-mcp

Allowed root:
  /home/user/src/worktrees/temote-mcp

Resolved path:
  /home/user/src/temote-mcp-feature

No worktree was created.
```

path collision の場合:

```text
Managed worktree path already exists.

Path:
  /home/user/src/worktrees/temote-mcp/issue-123

Action:
  reuse the existing worktree or choose a different task name
```

---

## Compatibility with sandbox / Developer Execution Broker

worktree creation は coding-agent sandbox 内で無理に実行するのではなく、Temote の Developer Execution Broker / host-side controlled operation として扱う。

これにより、coding agent から見た、

```text
.git/worktrees/*
.git/index.lock
worktree parent directory
```

等への permission friction を減らす。

workspace を作成した後、その workspace を許可された session root として coding agent に渡す。

---

## Security model

worktree broker は「Git コマンドを自由に実行できる escape hatch」にしない。

許可する operation と path を構造化された引数で制限する。

特に `worktree_create` に arbitrary shell command や arbitrary path を渡さない。

内部的に Git を実行する場合でも、Temote が導出・検証済みの値のみ使用する。

---

## Expected UX

ユーザーまたは orchestrator は原則として worktree の実 path を指定しなくてよい。

例:

```text
Implement issue #123 in a separate worktree.
```

Temote が、

```text
repository:
temote-mcp

branch:
feat/issue-123

workspace:
~/src/worktrees/temote-mcp/issue-123
```

を決定する。

coding agent には workspace の作成や配置ルールを説明する必要がない。

---

## Migration strategy

### Phase 1

- `~/src/worktrees` policy を定義する
- broker operation を追加する
- new worktree creation のみ managed root へ固定する
- legacy worktree はそのまま残す

### Phase 2

- session creation / agent mode と broker を統合する
- `local_agent_run` 起動時に managed workspace を自動選択できるようにする
- active session / running job と worktree lifecycle を連携する

### Phase 3

必要に応じて以下を追加する。

- stale managed worktree detection
- safe cleanup UX
- task / issue / branch からの deterministic naming
- managed worktree inventory
- collision resolution

legacy worktree の migration 自動化は必須ではない。

---

## Acceptance criteria

### Creation

- [ ] 新規 worktree が `~/src/worktrees/<repo>/<task>` に作成される。
- [ ] caller は arbitrary worktree path を指定できない。
- [ ] branch 名に `/` が含まれていても意図しない directory hierarchy が作られない。
- [ ] path traversal により managed root 外へ作成できない。
- [ ] symlink 等を介して managed root 外へ escape できない。

### Agent integration

- [ ] Temote が managed worktree を session root として利用できる。
- [ ] `local_agent_run` が managed worktree を cwd として起動できる。
- [ ] coding agent が worktree location policy を知らなくても正常に作業できる。
- [ ] worktree lifecycle が AI model の prompt compliance に依存しない。

### Existing workspaces

- [ ] legacy worktree を自動削除しない。
- [ ] legacy worktree を自動移動しない。
- [ ] 他作業者の dirty worktree を reset / stash / checkout しない。

### Removal

- [ ] dirty worktree が default で force remove されない。
- [ ] running job / active session が使用中の worktree を誤って削除しない。
- [ ] unrelated directory を削除対象にできない。

### Tests

- [ ] managed root 内の正常作成
- [ ] `../` path traversal rejection
- [ ] absolute path injection rejection
- [ ] symlink escape rejection
- [ ] duplicate task/path collision
- [ ] branch containing `/`
- [ ] dirty worktree removal guard
- [ ] active-session removal guard
- [ ] legacy worktree preservation

---

## Implementation packets

Do not hand this umbrella directly to OpenCode. Execute these bounded packets in roadmap order:

1. `issues/polished/20260916-managed-worktree-create-list.md`
2. `issues/polished/20260916-managed-worktree-session-integration.md`
3. `issues/done/20260916-structured-worktree-remove.md`
4. `issues/done/20260916-managed-worktree-prune.md`
5. `issues/done/20260916-git-shim-worktree.md`

Close this tracker only after all packets are done and legacy-worktree preservation has regression coverage.

---

## Final policy

```text
Canonical repositories:
  ~/src/<repo>

Managed worktrees:
  ~/src/worktrees/<repo>/<task>

Rules:
- AI coding agents do not choose worktree paths.
- AI coding agents do not own worktree lifecycle.
- Worktree lifecycle is owned by the Temote broker.
- Arbitrary worktree paths are not accepted by the broker.
- New worktrees are always created below ~/src/worktrees/<repo>/.
- Existing legacy worktrees are preserved until explicitly cleaned up.
- Temote sessions use the managed worktree as their project root.
- local_agent_run receives that project root and performs implementation work there.
- Repository-local AGENTS.md policy is not required for enforcing this behavior.
```

## 2026-09-22 consolidation: done

All bounded child packets are complete on `main` or in the merging PR set; remaining live acceptance is tracked in `issues/open/20260908-live-acceptance-matrix.md`.
