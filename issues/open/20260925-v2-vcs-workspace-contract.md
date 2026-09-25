# V2: backend-neutral VCS / workspace contract

Status: contract delivered / implementation not started  
Repository: `f4ah6o/temote-mcp`  
Parent: `issues/open/20260925-vcs-transaction-jj-first.md`  
Related: `issues/open/20260925-f1-repository-store-workspace-contract.md`, `issues/open/20260925-observation-context-memory-plane.md`  
Created: 2026-09-25 (Asia/Tokyo)

## 1. Goal

V1 の実測結果を反映し、Temote managed workspace を `git worktree` 固定から切り離す。

新規 managed workspace の第一候補は Jujutsu とし、Git は compatibility backend として残す。

この contract が固定するもの:

- repository identity / backing store
- workspace identity / ownership
- VCS backend selection / capability
- logical change / materialized revision
- snapshot / checkpoint / reconcile
- task / execution / VCS correlation
- delivery ref
- fallback / migration rules

この packet は実装コードを変更しない。

## 2. Fixed decision

### 2.1 Default candidate

```text
new managed workspace
  -> VcsBackend::Jujutsu
       unless repository capability rejects it

legacy / unsupported
  -> VcsBackend::Git
       only by explicit repository/caller policy
```

silent fallback は禁止する。

### 2.2 Generic repository contract retained from F1

以下は backend-neutral として維持する。

- RepositoryId = host / owner / name
- remote URL
- remote default branch
- last successful fetch / observed commit / observed_at
- freshness / stale / unconfirmed
- no-local-main policy
- existing dirty/ahead/diverged checkout preservation
- Temote reservation / writer ownership
- operation receipt / reconciliation

bare Git object store は GitHub delivery boundary と jj Git backend の共通 substrate として利用可能。

## 3. Workspace record

Conceptual machine-readable record:

```json
{
  "schema_version": 2,
  "repository": {
    "host": "github.com",
    "owner": "f4ah6o",
    "name": "example"
  },
  "workspace_id": "task-api",
  "backend": "jujutsu",
  "path": "/.../example.workspaces/task-api",
  "canonical_path": "/.../example.workspaces/task-api",
  "base": {
    "source": "remote-tracking",
    "requested": "origin/main",
    "resolved_commit": "<git-sha>"
  },
  "vcs": {
    "logical_change_id": "<jj-change-id>",
    "materialized_revision": "<jj-commit-id>",
    "operation_id": "<jj-operation-id>",
    "workspace_name": "task-api",
    "conflicted": false,
    "empty": false
  },
  "delivery": {
    "ref_name": null,
    "remote": "origin"
  },
  "freshness": {
    "observed_at": "...",
    "remote_commit": "<git-sha>",
    "state": "fresh"
  }
}
```

Git backend では `vcs` の backend-specific field を置換する。

## 4. Identity separation

以下を同一視しない。

```text
Temote Task ID
!= Temote Execution ID
!= Temote Workspace ID
!= jj workspace name
!= jj change_id
!= jj commit_id
!= jj operation_id
!= Git branch/bookmark
!= GitHub PR
```

Temote が mapping を所有する。

## 5. Jujutsu backend contract

### 5.1 Workspace

Temote `workspace_id` を jj workspace name へ明示 mapping する。

jj 自身が Temote 用の永続 workspace UUID を提供することを前提にしない。

### 5.2 Logical change

通常の mutating implementation task では 1 managed workspace に少なくとも 1 active logical change を持つ。

```text
logical_change_id = jj change_id
materialized_revision = jj commit_id
```

external file edit 後に snapshot が発生すると:

- `change_id` は logical continuity として維持可能
- `commit_id` は新しい content snapshot へ変化する

Temote は verification / delivery を `commit_id` 等の materialized revision に束縛する。

### 5.3 Snapshot

typed operation:

```text
vcs_snapshot(workspace_id)
```

minimum result:

```text
workspace_id
backend
logical_change_id
before_revision
after_revision
operation_id
changed
conflicted
empty
```

jj backend は ordinary jj command が working copy を snapshot する性質を利用してよいが、Temote は snapshot point を明示的な lifecycle event として扱う。

### 5.4 Snapshot points

最低限:

- execution start 前
- execution の significant state transition 後
- steer/resume 前後
- task get / reconcile
- interrupt / crash detection 後
- verification 前
- delivery 前
- workspace release 前

### 5.5 Operation log

jj operation ID は recovery correlation に利用する。

Temote operation receipt と jj operation ID は別物として mapping する。

```text
Temote operation_id -> VCS operation_id(s)
```

`jj undo` / earlier operation restore を一般 caller に raw expose しない。
recovery adapter の typed operation としてのみ利用する。

### 5.6 Author identity

V1 で author/committer 未設定 commit は push を拒否された。

Temote は jj workspace 作成時に repository-scoped identity を明示 wiring する。

source of identity は gh-git / repository binding と整合させる。

global jj user config の偶然の状態へ依存しない。

### 5.7 Delivery bookmark

delivery ref は active working-copy change と分離する。

```text
logical change
 -> verified materialized revision
 -> delivery bookmark
 -> explicit remote tracking
 -> push
 -> GitHub PR
```

new bookmark の remote tracking は explicit operation とする。
push failure を task success に読み替えない。

## 6. Git compatibility backend

Git backend は legacy / capability fallback。

minimum backend-specific state:

```text
branch
HEAD commit
index/working-tree state
ahead/behind/diverged
reflog/reference if recovery uses it
```

Git backend で「agent が commit を忘れても必ず recoverable」を満たす方式は V3 の別実装契約で固定する。

V2 では hidden auto-commit broker を暗黙導入しない。

## 7. Repository capability gate

Conceptual result:

```json
{
  "preferred_backend": "jujutsu",
  "supported": true,
  "reasons": [],
  "capabilities": {
    "jj_available": true,
    "git_backend": true,
    "submodules": "unknown",
    "git_lfs": "unknown",
    "shallow_clone": "supported",
    "required_git_hooks": "unknown",
    "colocated_git": "supported-with-caveats"
  }
}
```

states:

- `supported`
- `unsupported`
- `unknown`

unknown を supported と読み替えない。

### V1 evidence

Observed:

- jj 0.37.0
- Git 2.50.1 (Apple Git-155)
- shallow jj clone: PASS
- submodules: NOT RUN
- Git LFS: NOT RUN
- required hooks: NOT RUN

したがって submodule/LFS/hooks repository は初期 jj-first eligibility を conservative に扱う。

## 8. Git read-only compatibility

V1 の colocated fixture では `git status` は成立したが、Git ref/commit がまだ無い段階で:

- `git log`
- `git show HEAD`
- `git rev-parse HEAD`

は通常 Git repository と同じ意味では使えなかった。

Temote は「colocated == arbitrary Git tooling fully compatible」と仮定しない。

build/tooling が Git HEAD を要求する repository は capability gate で検出・fixture acceptance する。

## 9. Observation integration

Observation Plane へ以下を記録する。

```text
VcsSnapshotObserved {
  workspace_id,
  task_id,
  execution_id?,
  backend,
  logical_change_id?,
  before_revision,
  after_revision,
  vcs_operation_id?,
  conflicted,
  empty
}
```

raw diff 全文は observation metadata に複製しない。

instruction / execution / VCS revision / verification を correlation できること。

## 10. Verification binding

verification result は materialized revision に束縛する。

```text
Verification {
  workspace_id,
  revision,
  snapshot_operation,
  command/check identity,
  result
}
```

snapshot 後に revision が変わったら旧 PASS を current PASS と表示しない。

## 11. Reconciliation

reconcile は:

1. Temote workspace ownership を確認
2. backend-native current state を inspect
3. last known revision / operation と比較
4. missing observation を backfill
5. conflict / divergence / stale workspace を明示
6. 根拠なく reset / undo / recreate しない

jj backend で stale workspace が自動 rebase された場合も before/after revision を observation に残す。

## 12. Workspace isolation

1 Temote managed workspace = 1 backend-native writable working copy。

複数 task が同じ mutable working copy を共有しない。

同一 repository の sibling jj workspace は同じ object store / operation store を共有し得るが、Temote writer reservation は workspace ownership と repository-wide management operation を保護する。

## 13. gh-git boundary

### gh-git keeps

- GitHub account / identity selection
- repository identity / remote mapping
- credential routing
- Git-compatible fetch/push primitives where appropriate

### Temote owns

- VCS backend selection
- workspace/task binding
- jj workspace creation/removal adapter
- snapshot/checkpoint/reconcile
- writer reservation
- task/execution/revision correlation
- delivery operation coordination

gh-git を jj-specific workflow engine にしない。

## 14. F1 revision mapping

F1 の以下は generic として残す:

- repository identifier
- bare backing store
- remote-tracking freshness
- no local main
- legacy checkout protection
- reservation ownership

以下は V2 で置換:

```text
old:
  layout = store-worktree
  git worktree add
  branch required as workspace identity

new:
  backend = jujutsu | git
  backend-native workspace
  workspace_id is Temote identity
  delivery_ref is separate
  logical_change_id / materialized_revision are explicit
```

## 15. Public / internal surface

initial internal typed surface:

```text
vcs_capabilities
vcs_workspace_ensure
vcs_inspect
vcs_snapshot
vcs_diff_summary
vcs_reconcile
vcs_prepare_delivery
vcs_workspace_remove
```

raw `jj <argv>` / `git <argv>` public proxy は作らない。

## 16. Acceptance

- [ ] jj backend で external edit を explicit Git commit なしに snapshot できる
- [ ] same logical change / changed materialized revision を区別できる
- [ ] task/execution/revision mapping が永続 record に残る
- [ ] backend switch / head switch 後に same logical work を再発見できる
- [ ] repository-scoped author identity が delivery 前に保証される
- [ ] unsupported/unknown capability は silent fallback しない
- [ ] Git compatibility backend は既存 checkout を破壊しない
- [ ] verification が exact revision に束縛される
- [ ] VCS observation が Context Plane から参照可能
- [ ] no local main checkout を維持できる

## 17. Next packet

V3 は実装 packet。

V3 の最初の slice は jj adapter 全体ではなく、temporary fixture + typed parser を使って:

1. `vcs_capabilities`
2. `vcs_workspace_ensure`
3. `vcs_snapshot`
4. `vcs_inspect`

までを成立させる。

delivery / GitHub PR は V4 に残す。
