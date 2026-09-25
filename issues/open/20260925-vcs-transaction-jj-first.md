# V0: VCS transaction layer / jj-first managed workspace design

Status: high-priority design / implementation not started  
Repository: `f4ah6o/temote-mcp`  
Parent: `issues/open/20260924-temote-development-harness-restructure.md`  
Related: `issues/open/20260925-f1-repository-store-workspace-contract.md`, `issues/open/20260925-observation-context-memory-plane.md`  
Created: 2026-09-25 (Asia/Tokyo)

## 1. Problem

Temote が coding agent に実装を委譲すると、agent が file を変更したまま commit しない、別 agent の変更と混ざる、途中状態がどの instruction の結果か追えない、といった状態が頻発しうる。

この問題を「agent は作業後に必ず `git add && git commit` する」という prompt / discipline で解決しない。

Temote 側で VCS transaction boundary を持ち、

- task / execution ごとの変更所有権を明確にする
- agent が commit を忘れても変更を recoverable にする
- instruction / execution と filesystem change を correlation する
- delivery 用の Git history と作業途中の snapshot history を分ける
- 他 task / 他 agent の変更を暗黙に混ぜない

ことを目的とする。

## 2. Why evaluate Jujutsu

Jujutsu は Git backend と相互運用しつつ、working copy を commit として扱う。

公式仕様上:

- working-copy changes はほぼ全ての `jj` command の開始時に自動 snapshot される
- added files は ignore 対象でなければ自動 tracking される
- staging area を前提にしない
- native `jj workspace` で 1 repository に複数 working copy を持てる
- `git worktree` 自体は Jujutsu の managed workspace primitive ではない
- bare Git repository を backend に利用できる
- colocated Git/JJ workspace では Git-compatible tools を残せるが、mutating Git と mutating jj の混在は最小化した方が状態を追いやすい
- operation log / undo と working-copy revision がある

この性質は「agent に commit maintenance をさせない」という Temote の目的と合う。

## 3. Two candidate architectures

### Option G — Git worktree + Temote snapshot/commit broker

```text
bare Git store
   |
git worktree per task
   |
agent edits
   |
Temote detects dirty state
   |
Temote stages/commits snapshots
```

必要になるもの:

- dirty detection
- add policy
- ignored/untracked policy
- commit author / message policy
- partial staging policy
- crash between edit and commit handling
- concurrent Git command arbitration
- reset/rebase/recovery policy
- temporary snapshot commits vs delivery commits の区別

利点:

- 現行 F1/gh-git contract をほぼ維持できる
- Git-only tooling と互換性が高い
- team の既存 mental model に近い

欠点:

- Temote が Git staging/commit semantics をかなり再実装する
- agent が直接 mutating Git を行うと broker と競合する
- filesystem change は commit されるまで authoritative snapshot にならない
- crash / abandoned edit の回収を Temote 独自ロジックで保証する必要がある
- 「未commitを構造的になくす」より「未commitを頻繁にcommitする」設計になる

### Option J — jj workspace + working-copy change

```text
Git-backed jj repository
   |
jj workspace per Temote workspace
   |
working-copy commit / change
   |
agent edits
   |
Temote snapshot boundary (jj command)
   |
same logical jj change evolves
   |
delivery boundary
   |
bookmark / Git refs / GitHub PR
```

利点:

- working copy が VCS object なので「dirty but unsaved」という状態を小さくできる
- staging / add / commit forgetting を normal path から除去できる
- logical `change_id` を Task / Execution と correlation できる
- operation log / undo が recovery substrate になる
- task workspace isolation を `jj workspace` に自然に写像できる
-作業 snapshot history と delivery history を分離しやすい

欠点 /制約:

- 現行 F1 の `git worktree` contract をそのまま使えない
- Git submodules / LFS 等、jj の未対応/制約機能がある
- mutating Git と jj を同じ colocated workspace で自由に混ぜる設計は避ける必要がある
- gh-git / gh-stack の責務を再整理する必要がある
- jj 自体を managed dependency / capability として扱う必要がある

## 4. Decision direction

**新規 Temote-managed workspace の target architecture は jj-first を優先して検証する。**

ただし即座に F1 を破棄しない。

最初に Temote core へ `VcsBackend` / `WorkspaceVcs` boundary を入れ、Git worktree と jj workspace の両方を表現できる contract にする。

初期 target:

```text
RepositoryStore
    |
    +-- VcsBackend::Jujutsu     # new managed default candidate
    |      +-- jj workspace
    |
    +-- VcsBackend::Git        # legacy / compatibility
           +-- git worktree
```

V1 acceptance を満たしたら、新規 managed workspace の default を Jujutsu とする。
満たせない repository capability (例: unsupported feature) は Git backend に明示 fallback し、silent fallback はしない。

## 5. Core abstraction

Conceptual types:

```rust
enum VcsBackendKind {
    Jujutsu,
    Git,
}

struct WorkspaceVcs {
    backend: VcsBackendKind,
    repository: RepositoryId,
    workspace_id: WorkspaceId,

    base_revision: RevisionRef,
    current_revision: RevisionRef,

    // jj backend
    change_id: Option<ChangeId>,
    operation_id: Option<VcsOperationId>,

    // delivery
    branch_or_bookmark: Option<String>,
}

trait VcsBackend {
    ensure_repository(...);
    ensure_workspace(...);
    inspect(...);
    snapshot(...);
    diff(...);
    checkpoint(...);
    reconcile(...);
    prepare_delivery(...);
    remove_workspace(...);
}
```

Public caller に raw argv を渡させない。
Temote の fixed adapter が typed operation を生成する。

## 6. Task / Execution / VCS relationship

Task と VCS change を同一 ID にしない。

```text
Task
  |
  +-- Workspace
  |      |
  |      +-- VCS state
  |             change_id / revision
  |
  +-- Execution #1 Codex
  |      before_revision
  |      after_revision
  |
  +-- Execution #2 OpenCode
         before_revision
         after_revision
```

jj backend では 1 Task / Workspace の logical work を 1 change_id に対応させることを default candidate とする。

execution が変わっても同じ logical change を引き継げる。
ただし review-only / investigation-only execution のように filesystem mutation を意図しない task は change を必須にしない。

## 7. Snapshot boundary

jj は filesystem watcher ではないため、Temote が explicit snapshot points を持つ。

最低限:

1. task / execution start 前
2. backend turn / task state の重要 transition 後
3. `task_get` / reconcile 時
4. steer / resume の前後
5. interrupt / backend crash 検出時
6. verification 前
7. delivery 前
8. workspace release / stop 前

snapshot は「delivery commit 作成」と同義にしない。

jj backend:
- `jj status` 等の snapshot-capable operation で working copy を repository state に取り込む
- change_id / commit_id / jj operation id を再読して record

Git backend:
- dirty state を観測
- V1 では automatic hidden commit broker を実装せず、fallback semantics を別 contract で決める

## 8. VCS Observation

Observation Plane に VCS observation を追加する。

```text
VcsSnapshotObserved {
  task_id
  execution_id?
  workspace_id
  vcs_backend
  before_revision
  after_revision
  change_id?
  vcs_operation_id?
  changed_paths_digest
  dirty_or_conflicted
}
```

raw patch 全文を ordinary observation metadata に複製しない。
必要な diff は bounded evidence / VCS-native object から authorization を再確認して取得する。

これにより:

```text
instruction
  "CIを直して"
       |
       v
ExecutionStarted
       |
       v
VcsSnapshotObserved
  A -> B
       |
       v
VerificationPassed
```

を correlation できる。

## 9. Agent mutation policy

jj-first managed workspace では、agent に mutating Git を normal contract として要求しない。

推奨 policy:

- read-only Git: 許可
  - `git status`
  - `git diff`
  - `git log`
  - `git show`
  - `git rev-parse`
- normal VCS mutation:
  - Temote/jj adapter が所有
- delivery mutation:
  - Temote delivery adapter が所有

特に以下を agent prompt の規律だけに依存しない:

- `git commit`
- `git reset`
- `git rebase`
- `git push`
- branch/base mutation

ただし sandboxed agent が repository 内で Git binary を完全に実行不能にするかは V1 で実測し、toolchain/build systems が必要とする read-only Git を壊さない方法を選ぶ。

## 10. Delivery model

working snapshot と GitHub delivery history を分ける。

```text
jj logical change(s)
       |
verification
       |
delivery preparation
       |
describe / squash / split if policy requires
       |
bookmark / Git ref
       |
push
       |
GitHub PR
```

delivery は task completion の副作用として自動実行しない。
既存 Phase E authorization / receipt / reconciliation contract を維持する。

### Stacked delivery

現行 `gh-stack` integration は再評価対象。

jj-first では task/change graph から explicit delivery branch/bookmark graph を生成し、
GitHub API / supported tooling で PR base relationship を反映する方式を比較する。

`gh stack init/add/checkout/rebase/sync` のような checkout-mutating workflow を jj-managed workspace の normal path に混ぜない。

`gh stack link` を delivery-only adapter として利用可能かは V2 で実測する。

## 11. Repository capability gate

Jujutsu を全 repo に強制しない。

`VcsCapability` を inspect する。

候補:

```text
git_backend_available
jj_available
submodules_present
git_lfs_present
partial_clone
shallow_clone
required_git_hooks
colocation_compatible
```

repository が jj managed mode を満たさない場合:

- reason を machine-readable に返す
- Git backend を選ぶには caller / repository policy で明示
- silently Git mode に落とさない

## 12. Interaction with gh-git

gh-git を捨てない。

責務を:

```text
gh-git:
  GitHub identity / credential routing
  remote/repository identity
  Git-compatible remote primitive

Temote VCS:
  workspace ownership
  VCS backend selection
  snapshot/checkpoint/reconcile
  task/change correlation

jj:
  working-copy / change / operation substrate
```

に再整理する。

F1 の store identity / freshness / origin/main observation contract は再利用可能。
`git worktree` 固定部分だけを backend-specific として切り離す。

## 13. Interaction with Observation / Context Plane

VCS は memory worker の source of truth にしない。
Temote authoritative state + VCS observations の一つとして使う。

Memory Worker は例えば:

-「この指示でどの revision range が変わったか」
-「同じ failure を何度修正したか」
-「前の head がどこまで変更を残したか」

を support refs 付きで整理できる。

coding agent に commit summary / handoff summary を強制しない。

## 14. Migration

existing checkout / dirty state は絶対に自動破棄しない。

legacy Git workspace の jj 変換を自動実行しない。

migration candidate は:

1. inspect
2. dirty / ahead / diverged / submodule / LFS 等を report
3. safe eligibility を判定
4. explicit migration operation
5. before/after refs を evidence に保存

既存 Git worktree を `jj workspace` として勝手に adopt / move / delete しない。

## 15. Priority

この track は **F2/F3/C1 より前に V1 prototype を行う high-priority design correction** とする。

理由:

- F2/F3 で git worktree implementation を固定すると後から workspace substrate の置換コストが高い
- Observation Plane と VCS correlation point を同時に決められる
- 未commit残留は実際の運用 friction に直結する
- agent に commit discipline を追加する方向は Temote の head-independent design と逆行する

A / O の共通 identity work は継続してよい。
F1 の repository identity / bare store / freshness design も維持可能な部分は継続する。
ただし **F2/F3 の git-worktree-specific implementation は V1 decision まで開始しない**。

## 16. Implementation packets

### V0 — design comparison (this document)

- [x] Git broker vs jj-first 比較
- [x] VCS abstraction
- [x] snapshot boundary
- [x] observation integration
- [x] delivery boundary
- [x] migration / fallback policy

### V1 — jj feasibility prototype

Goal: user repository を変更せず temporary fixture で jj-first managed workspace が Temote requirements を満たすか実測する。

- [ ] installed/pinned jj version / invocation contract
- [ ] bare Git backend + jj repository
- [ ] 2 independent `jj workspace`
- [ ] task change_id を作成/保持
- [ ] external file edit → Temote-triggered snapshot
- [ ] agent/process crash相当の edit → 次 snapshot で recovery
- [ ] concurrent workspace isolation
- [ ] operation log / undo / stale workspace behavior
- [ ] Git read-only tooling compatibility
- [ ] bookmark/export/push-equivalent to temporary local Git remote
- [ ] no local main checkout
- [ ] no user dirty checkout mutation
- [ ] JSON/template output stable enough for typed adapter

V1 acceptance に失敗した項目は、jj を採用するために Temote が workaround を抱えるコストも記録する。

### V2 — workspace contract revision

V1 PASS 後:

- [ ] F1 の generic parts (RepositoryId / freshness / no-local-main) を保持
- [ ] `layout=store-worktree` を generic workspace backend field へ変更
- [ ] jj workspace JSON contract
- [ ] Git compatibility backend contract
- [ ] gh-git responsibility update
- [ ] C1/C2 を backend-neutral に更新

### V3 — Temote VCS adapter

- [ ] typed `VcsBackend`
- [ ] snapshot/checkpoint/reconcile
- [ ] Task/Execution/VCS correlation
- [ ] VCS Observation hook
- [ ] workspace lifecycle
- [ ] capability gate
- [ ] no mutating raw argv surface

### V4 — delivery integration

- [ ] jj changes → delivery bookmarks/refs
- [ ] single PR
- [ ] stacked PR strategy decision
- [ ] remote receipt/reconciliation
- [ ] final verification binds to delivered revision

## 17. Acceptance criteria

- [ ] agent が explicit commit を実行しなくても、Temote は task workspace の変更を recoverable VCS state として捕捉できる
- [ ] task/execution と before/after revision を correlation できる
- [ ] head/backend を切り替えても同じ logical work を引き継げる
- [ ] concurrent task が同じ mutable working copy を共有しない
- [ ] snapshot と GitHub delivery commit/PR を分離できる
- [ ] crash後の未commit filesystem changes を黙って破棄しない
- [ ] existing dirty/ahead/diverged Git checkout を migration のために reset/clean/stash しない
- [ ] jj unsupported repository は reason を明示し、silent fallback しない
- [ ] GitHub remains Git-compatible at delivery boundary
- [ ] agent に memory/commit/handoff maintenance prompt を要求しない
- [ ] Observation Plane が instruction → execution → VCS revision → verification を相関できる

## 18. Principle

> Agent に「commit を忘れないで」と頼むのではなく、working state 自体を Temote の管理対象にする。

新規 managed workspace では Jujutsu の working-copy commit / change / operation model を第一候補とし、Temote はその上に task ownership、authorization、observation、verification、delivery を載せる。
