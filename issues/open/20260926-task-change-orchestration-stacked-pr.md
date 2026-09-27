# Task / Change graph を orchestration と stacked PR delivery の source of truth にする

Status: design ready / implementation not started  
Repository: `f4ah6o/temote-mcp`  
Parent: `issues/open/20260925-vcs-transaction-jj-first.md`  
Umbrella: `issues/open/20260924-temote-development-harness-restructure.md`  
Related: `issues/open/20260925-v2-vcs-workspace-contract.md`  
Created: 2026-09-26 (Asia/Tokyo)

## 1. Problem

Agent orchestration の runtime tree と GitHub stacked PR の dependency graph は、見た目が同じ tree になることはあるが意味が違う。

```text
agent A
└─ subagent B
   └─ subsubagent C

PR A
└─ PR B
   └─ PR C
```

上記を常に 1:1 対応として強制すると誤る。

- `agent -> subagent` は execution / delegation / context の関係。
- stacked PR は durable code change の依存関係。
- subagent が親 task の調査・レビュー・補助だけを行うなら新しい PR は不要。
- sibling agent として並列起動していても、一方の変更が他方の未merge変更を必要とするなら PR は stack になる。
- agent/backend は logical work の途中で Codex / OpenCode / Devin 間を交代し得る。agent lifetime を Git history identity にしてはならない。

したがって **agent hierarchy と change / PR hierarchy を同一視しない**。

## 2. Decision

Temote は **Task / Change dependency graph を source of truth** とし、次の2つを別々に派生させる。

```text
                        Task / Change graph
                         /             \
                        /               \
              executor assignment       delivery graph
             agent / subagent /      single / sibling /
             replacement agent         stacked PR
```

固定する原則:

> Agent hierarchy != Git hierarchy.
>
> Task / Change dependency -> VCS / delivery hierarchy.
>
> Agent is an executor assigned to a durable unit of work.

## 3. Why enforce this in Temote

Temote が強制すべき対象は「subagent は必ず child PR」のような形ではなく、**mutating work が誰の workspace を汚しているか曖昧にならないこと**。

避けたい状態:

```text
agent A edits checkout
  -> subagent B edits same checkout
  -> subsubagent C edits same checkout
  -> unrelated uncommitted changes accumulate
  -> ownership / delivery boundary becomes unclear
```

target:

```text
durable change
  -> isolated managed workspace
  -> executor assignment
  -> snapshot / verification
  -> delivery
```

executor を先に作って workspace を後付けするのではなく、独立した mutating unit では **change / workspace を先に確保してから executor を割り当てる**。

## 4. Identity model

既存 V2 contract と同様に、以下を同一視しない。

```text
Temote Task ID
!= Temote Change ID
!= Temote Execution ID
!= Temote Workspace ID
!= jj workspace name
!= jj change_id
!= jj commit_id
!= jj operation_id
!= Git bookmark / branch
!= GitHub PR
!= GitHub Stack object
```

Temote が mapping を所有する。

### 4.1 Conceptual records

```text
Task {
  task_id
  parent_task_id?
  ...
}

Change {
  change_id
  task_id
  parent_change_id?
  base:
    origin/main | change_id
  workspace_id
  logical_change_id?          # jj change_id
  materialized_revision?
  delivery_ref?
  pull_request?
}

Execution {
  execution_id
  task_id
  change_id?
  backend
  executor_identity
  ...
}
```

重要:

```text
parent_task_id
!= necessarily parent_change_id
```

Task tree は delegation を表現できるが、それだけから delivery dependency を推測しない。

## 5. Change creation rule

mutating task を独立した durable change として扱う場合:

1. Temote Change ID を発行する。
2. explicit base を決める。
3. isolated managed workspace を確保する。
4. VCS backend の logical change identity を作成 / 発見する。
5. その後で executor を割り当てる。

概念 API:

```text
delegate(
  task,
  base_change = current_change | origin/main | other_change
)
```

`delegate` は agent hierarchy だけを作る API にしない。
mutating child なら change allocation policy を明示的に通す。

## 6. Reuse parent change vs create child change

subtask ごとに PR を作らない。

decision:

```text
independent review unit?
  |
  +-- no  -> reuse parent change/workspace under serialized ownership
  |
  +-- yes -> create distinct change/workspace
                |
                +-- depends on unmerged change?
                |      |
                |      +-- yes -> stacked PR candidate
                |
                +-- no -> sibling/single PR from origin/main
```

「independent review unit」は少なくとも:

- parent から分離して verification できる
- parent から分離して revert / omit できる
- delivery dependency を explicit に記述できる

ことを要求する。

## 7. Workspace invariant

### 7.1 Independent writable change

```text
1 independently writable Change
  -> 1 writable managed workspace
```

複数の independently writable change が同じ mutable working copy を共有しない。

### 7.2 Shared parent change

調査・レビュー・計画・read-only subagent は child workspace を要求しない。

同じ parent change を複数 executor が編集する必要がある場合は、同時 writer として扱わず Temote の reservation / ownership handoff を通す。

agent nesting を workspace sharing の許可として扱わない。

## 8. Executor replacement

Change の identity は executor より長命。

例:

```text
Change Y
  execution 1: OpenCode
  execution 2: Codex
  execution 3: Devin
```

backend/head を切り替えても `Change Y` は同じ logical work として継続できる。

Observation / Context Plane は:

```text
Task
  -> Change
  -> Execution(s)
  -> VCS snapshots
  -> Verification
  -> Delivery
```

を correlation できるようにする。

## 9. Delivery graph

delivery topology は agent topology ではなく change dependency から生成する。

### 9.1 Single / sibling PR

```text
origin/main
├─ Change A -> PR A
└─ Change B -> PR B
```

A/B の executor が親子でも兄弟でも、Change B が A に依存しなければ stack しない。

### 9.2 Stacked PR

```text
origin/main
  |
Change A -> PR A
  |
Change B -> PR B (base = A delivery ref)
  |
Change C -> PR C (base = B delivery ref)
```

必要条件:

- child change の base dependency が explicit
- dependency target が未merge
- verified materialized revision が delivery ref に固定されている
- delivery adapter が remote state を reconcile できる

## 10. gh-stack boundary

`gh-stack` は **delivery adapter** として使う。

`gh-stack` に task/change truth を持たせない。

Temote owns:

- Task / Change dependency
- workspace ownership
- jj change / revision correlation
- delivery planning
- expected PR base graph
- operation receipt / reconciliation

`gh-stack` adapter owns:

- eligible delivery refs の push
- PR create/update
- stack link/update
- actual remote PR base / Stack object observation

Temote は expected graph と actual GitHub graph を比較し、drift を状態として保持する。

`gh stack init/add/checkout/rebase/sync` のように local checkout を所有する workflow は managed workspace ownership と衝突し得るため、初期 adapter では採用しない。
既存調査どおり `gh stack link` 中心を候補とする。

## 11. Delivery plan

mutation 前に machine-readable plan を作る。

概念例:

```json
{
  "change_id": "chg_B",
  "revision": "<verified revision>",
  "delivery": {
    "kind": "stacked_pr",
    "base_change_id": "chg_A",
    "base_ref": "temote/chg_A",
    "head_ref": "temote/chg_B"
  }
}
```

plan fingerprint を operation receipt に持たせ、retry で別 topology を作らない。

## 12. Reconciliation

delivery mutation 後に応答が失われても、blind retry しない。

最低限:

1. operation receipt を確認
2. local expected change graph を確認
3. GitHub 上の head/base PR graph を観測
4. matching PR / Stack object を再発見
5. expected state と一致すれば backfill
6. 不一致なら `reconciliation_required`
7. 根拠なく branch force-update / PR retarget / stack rebuild をしない

## 13. Failure handling

以下を成功扱いしない。

- executor completed だが snapshot が無い
- verification が古い revision に対する PASS
- push 成功 / PR create 失敗
- PR create 成功 / stack link 応答不明
- expected base と actual PR base が不一致
- parent change merge/rebase 後に child verification が stale
- `gh-stack` remote object が見つからない

execution / verification / delivery state は既存方針どおり分離する。

## 14. Interaction with jj

jj は Change の source of truth ではなく VCS substrate。

typical mapping:

```text
Temote Change ID
  -> workspace_id
  -> jj workspace
  -> jj change_id
  -> materialized commit_id
  -> verified revision
  -> delivery bookmark
  -> GitHub PR
```

jj `change_id` は durable continuity に利用できるが、Temote Change ID の代わりにはしない。

1 Temote Change に複数 jj change が必要になる高度なケースを将来許容できるよう、schema 上は 1:1 を永久 invariant にしない。

## 15. Agent orchestration contract

agent tree は execution relationship として別途観測する。

```text
orchestrator
├─ agent A
│  └─ subagent C
└─ agent B
```

change graph:

```text
origin/main
├─ Change A
│  └─ Change C
└─ Change B
```

この2つが偶然一致しても、それを invariant にしない。

subagent creation は implicit に:

- branch を作らない
- jj change を作らない
- workspace を作らない
- PR を作らない

mutating durable work として delegate された場合のみ change allocation を行う。

## 16. Persistence minimum

最初の実装では少なくとも以下を durable record にする。

- `change_id`
- `task_id`
- optional `parent_change_id`
- base kind / base change
- `workspace_id`
- backend-native logical change identity
- latest materialized revision
- executor history / execution IDs
- verification target/result
- delivery plan
- delivery operation receipt
- PR identity / observed base
- reconciliation state

## 17. Implementation packets

### D0 — contract

この document。

- [x] agent graph と change graph を分離
- [x] Change identity を executor より先に置く
- [x] workspace invariant
- [x] delivery topology decision
- [x] gh-stack responsibility boundary
- [x] reconciliation requirement

### D1 — Change record / correlation

- [ ] persistent Temote Change record
- [ ] Task / Execution / Workspace / VCS mapping
- [ ] executor history
- [ ] observation correlation

### D2 — change/workspace allocation

- [ ] mutating delegate の change allocation
- [ ] explicit base-change contract
- [ ] isolated workspace ensure
- [ ] reuse-parent-change path
- [ ] writer handoff / reservation

### D3 — delivery planner

- [ ] single / sibling / stacked decision
- [ ] verified revision binding
- [ ] delivery ref naming
- [ ] deterministic plan + fingerprint
- [ ] parent merge/rebase stale detection

### D4 — GitHub / gh-stack adapter

- [ ] single PR adapter
- [ ] stacked PR via bounded `gh-stack` integration
- [ ] expected/actual graph reconciliation
- [ ] remote operation receipts
- [ ] uncertain result recovery

### D5 — lifecycle integration

- [ ] executor replacement preserves Change identity
- [ ] automatic snapshot before verification/delivery
- [ ] delivery status exposed through orchestration views
- [ ] final verification bound to delivered revision
- [ ] workspace release blocked while unreconciled delivery remains

## 18. Acceptance criteria

- [ ] agent/subagent hierarchy can differ from change/PR hierarchy without ambiguity
- [ ] every independently delivered mutating unit has a stable Temote Change ID
- [ ] same logical change survives executor/backend replacement
- [ ] independently writable changes never share the same mutable workspace
- [ ] stack/sibling/single-PR decision is derived from explicit change dependency
- [ ] subagent creation does not implicitly create VCS or GitHub objects
- [ ] jj/Git/GitHub identities remain distinct and correlated
- [ ] `gh-stack` state is reconciled as delivery state, not used as orchestration state
- [ ] verification is bound to the exact delivered revision
- [ ] uncertain remote delivery does not blind-retry into duplicate PRs/stacks
- [ ] no-local-main policy remains intact
- [ ] existing dirty/ahead/diverged checkout is never reset/cleaned/stashed to satisfy this flow

## 19. Principle

> Task / Change graph is the durable work model. Agent hierarchy is execution topology. PR stack is delivery topology.

Temote は executor の親子関係を Git に焼き付けるのではなく、durable change ownership と dependency を明示し、その graph から安全に workspace と delivery を派生させる。
