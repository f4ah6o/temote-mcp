# O0: head-independent observation / context / memory plane

Status: high-priority design / implementation not started  
Repository: `f4ah6o/temote-mcp`  
Parent: `issues/open/20260924-temote-development-harness-restructure.md`  
Priority: high — start contract work in parallel with Phase B/F; implementation hooks follow the common Task/Execution identity from Phase A  
Created: 2026-09-25 (Asia/Tokyo)

## 1. Goal

Temote を使う指示役 (ChatGPT、Codex、OpenCode、Devin、人間、将来の coordinator) を切り替えても、

- 何を誰/どの backend に依頼したか
- どの task / execution / workspace に関する依頼だったか
- 実際に何が起き、何が確認され、何が未確認か
- 過去にどの判断・事実・失敗・制約が得られ、現在も有効か

を共有できるようにする。

coding agent 自身には「覚える」「要約する」「memory を更新する」といった追加作業を要求しない。
Temote の observable boundary で自動的に観測し、専用 worker が非同期に整理する。

この機能は Temote の execution semantics と密接に結び付くが、planner / generic workflow / agent inbox を Temote core に持ち込むことを目的としない。

## 2. Product boundary

### 2.1 Temote core

Temote core は引き続き **正しく実行すること**を担当する。

- Task / Execution / Workspace identity
- authorization / permission
- operation receipt / idempotency
- backend lifecycle
- evidence / verification / delivery
- reconciliation

### 2.2 Observation plane

Observation plane は **何が観測されたかを記録すること**を担当する。

- normalized instruction
- task / execution / workspace linkage
- backend target
- state transition
- evidence / verification / delivery reference
- caller-visible backend result / error
- provenance / timestamps / revision

### 2.3 Memory worker

Memory worker は **観測から再利用可能な knowledge を導出すること**を担当する。

- current facts
- decisions
- constraints
- unresolved questions
- failure patterns
- project / repository summaries
- supersession relationships

worker の出力は source of truth ではない。raw observation と Temote の authoritative state から再生成可能な projection とする。

### 2.4 Context resolver

Context resolver は **次の head に必要な情報だけを返すこと**を担当する。

planner ではない。task 分解、実装方針、backend/model 自動選択は行わない。

## 3. Architecture

```text
 Head / caller
 ChatGPT / human / Codex / other coordinator
                 |
                 v
      frontend / transport
 MCP / local / HTTP / Gateway
                 |
                 v
       normalized core request
                 |
          +------+------+
          | Observation |
          |  Recorder   |
          +------+------+
                 |
                 +------------------------+
                 |                        |
                 v                        v
          Temote Core               Observation Store
                 |                        |
                 v                        v
          Backend Adapter             Memory Worker
 Codex / OpenCode / Devin / ...          |
                 |                        v
                 +-----------------> Knowledge Store
                                          |
                                          v
                                   Context Resolver
                                          |
                                          +----> next head
```

「proxy」は wire transport の proxy として実装しない。
MCP/local/HTTP/Gateway が normalization を終えて Temote core に入る共通境界で observation を作る。

理由:

1. transport ごとの重複実装を避ける。
2. session / task / execution / workspace / operation_id が解決済みである。
3. caller text と実際の execution state を同一視しない。
4. authorization / secret handling を既存 core と共有できる。

## 4. What is observed

### 4.1 Instruction observation

少なくとも次を記録する。

```text
who/where:
  caller identity / frontend / transport

what:
  task_start / steer / resume / interrupt / workspace / verification / delivery

target:
  selected backend / execution target

scope:
  session / repository / workspace / task / execution

correlation:
  operation_id / request fingerprint / parent task / continue_from

content:
  instruction reference + bounded preview/digest where allowed

time:
  accepted_at / observed_at
```

「なににどんな指示をしたか」を後で復元できることが acceptance である。

### 4.2 Execution observation

caller / agent の主張とは別に、Temote が実際に確認した state を記録する。

例:

```text
TaskCreated
WorkspaceBound
OperationAccepted
ExecutionStarted
ExecutionWaitingForInput
EvidenceRecorded
ExecutionCompleted
ExecutionFailed
VerificationRecorded
DeliverySubmitted
ReconciliationRequired
Reconciled
```

### 4.3 Conversation capture boundary

取得対象は Temote から観測可能なものだけ。

- Temote に渡された caller request
- backend に渡した structured task / control instruction
- backend から返された caller-visible result / state
- Temote の state transitions / evidence references

取得しないもの:

- head の hidden chain-of-thought
- backend の非公開 internal reasoning
- Temote を通過していない外部 chat 全体

「すべて」は Temote の observable execution boundary 内のすべてを意味する。

## 5. Raw observation model

最初から generic memory event schema にしない。
Temote identity と execution semantics を保持する小さい envelope にする。

Conceptual schema:

```rust
struct Observation {
    id: ObservationId,
    schema_version: u32,
    observed_at: Timestamp,

    session_id: SessionId,
    repository: Option<RepositoryId>,
    workspace_id: Option<WorkspaceId>,
    task_id: Option<TaskId>,
    execution_id: Option<ExecutionId>,
    operation_id: Option<OperationId>,

    actor: ActorRef,
    target: TargetRef,
    kind: ObservationKind,

    content: ContentRef,
    state_ref: Option<StateRef>,
    evidence_refs: Vec<EvidenceRef>,

    provenance: Provenance,
    revision: u64,
}
```

`ObservationKind` は最初は closed enum:

- `instruction`
- `operation_accepted`
- `execution_state`
- `evidence`
- `verification`
- `delivery`
- `reconciliation`

worker 固有の `fact` / `decision` 等を raw observation enum に混ぜない。

## 6. Content storage and security

既存 safety invariant を維持する。

- secret / credential / token を observation metadata、approval summary、ordinary output に複製しない。
- structured secret field は記録対象から除外する。
- task / steer 本文は既存 authoritative task record / bounded evidence を優先して **reference** し、同じ本文を observation journal に無制限複製しない。
- content body が必要な場合は session ownership / scope を再検証した Context Resolver 経由で解決する。
- observation listing は本文をデフォルトで inline しない。
- retention / deletion は raw observation と derived knowledge を別に扱う。
- worker に渡す前にも credential-bearing structured field は除外する。

free-text 内に利用者が秘密情報を書いた場合を完全検出できるとは仮定しない。
そのため O1 では「全文を何でも別ログへコピーする」実装を禁止し、canonical task/evidence への reference-first とする。

## 7. Raw observations vs derived knowledge

```text
authoritative state
  Temote Task / Execution / Evidence
            |
            +---------+
                      v
              Raw Observation
              append-only-ish
                      |
                      v
                Memory Worker
                      |
                      v
              Derived Knowledge
```

Raw observation:

- worker によって書き換えない
- provenance を失わない
- worker model を変更しても同じ入力にできる

Derived knowledge:

- dedupe 可
- supersede 可
- confidence / support refs を持つ
- worker を交換して再構築可

## 8. Knowledge model

最初の knowledge kind:

- `fact`
- `decision`
- `constraint`
- `observation`
- `failure_pattern`
- `unresolved`
- `summary`

Conceptual schema:

```rust
struct KnowledgeItem {
    id: KnowledgeId,
    kind: KnowledgeKind,
    scope: KnowledgeScope,
    text: String,

    support: Vec<ObservationId>,
    related_tasks: Vec<TaskId>,
    related_repositories: Vec<RepositoryId>,

    status: KnowledgeStatus,
    confidence: Option<f32>,

    valid_from: Option<Timestamp>,
    valid_until: Option<Timestamp>,
    supersedes: Vec<KnowledgeId>,

    producer: WorkerRef,
    produced_at: Timestamp,
}
```

`KnowledgeStatus`:

- `candidate`
- `supported`
- `current`
- `superseded`
- `retracted`

worker の抽出結果だけで verified execution fact に昇格させない。
Temote state / evidence に裏付けられるものと、caller/agent の claim を区別する。

## 9. Scope

Knowledge scope を最初から明示する。

```text
user
repository
workspace
task
execution
```

初期 implementation の retrieval は repository + task を中心にする。

重要な invariant:

- task 固有の一時情報を repository-wide current fact に自動昇格しない。
- repository A の knowledge を repository B へ暗黙共有しない。
- stale task state を current repository state として返さない。
- caller identity / authorization を越えて context を漏らさない。

## 10. Memory worker

### 10.1 Agent burden

coding backend は memory worker を呼ばない。
task prompt に memory maintenance instruction を追加しない。

Observation Store に新しい revision が増えたことを worker が追跡する。

### 10.2 Worker execution

worker は interactive coding task から分離する。

初期要件:

- cheap / replaceable model を利用可能
- batch 処理可能
- retry safe
- checkpoint (`last_observation_revision`) を保持
- 同じ batch の再実行で duplicate knowledge を増殖させない
- failure は coding task の成功/失敗へ影響させない

Temote core は特定の model provider に依存しない。
worker adapter / model configuration は後続 packet で固定する。

### 10.3 Worker output rules

worker は次を行う。

1. related observations を episode にまとめる。
2. candidate fact / decision / constraint / unresolved を抽出する。
3.既存 knowledge との重複を検査する。
4.矛盾する current item があれば supersession candidate を作る。
5. support observation IDs を必須にする。
6. unsupported assertion を current fact にしない。

## 11. Context resolver

Head は raw log 全件を読む必要がない。

Conceptual API:

```text
context_resolve(
  session_id,
  repository?,
  workspace_id?,
  task_id?,
  query?,
  budget?
)
```

Result:

```text
current_summary
relevant_decisions[]
relevant_facts[]
constraints[]
known_failure_patterns[]
unresolved[]
recent_related_tasks[]
refs[]
```

全 field に provenance / support reference を保持できること。

初期 retrieval は deterministic filter + text search から始めてよい。
embedding / vector DB は必須条件にしない。

## 12. Head switch acceptance

代表シナリオ:

```text
ChatGPT
  -> Temote task A -> Codex

later

OpenCode / another coordinator
  -> context_resolve(repo, task B)
```

task B の head が少なくとも以下を取得できる:

- task A に何を依頼したか
- backend は何だったか
- task A の observed final state
- verification の有無
- task A から抽出された current decisions / facts
- unresolved items
- support refs

元 head に memory 書込みを依頼していないこと。

## 13. Watch / revision integration

Observation Store は global append count ではなく、少なくとも scope 内で monotonic に追跡可能な revision/cursor を返す。

例:

```text
observation_list --after-revision 381
context_resolve --at-least-revision 381
```

将来の `task watch` / state revision と相互参照できるようにするが、同じ revision sequence に無理に統合しない。

## 14. Failure semantics

- Observation write failure after operation acceptance:
  - backend operation を盲目的に replay しない。
  - operation receipt / authoritative task state を優先する。
  - observation gap を明示し、reconciliation で backfill 可能にする。
- Worker failure:
  - raw observations を維持する。
  - context freshness を `stale` / last processed revision で示す。
  - task execution を failed にしない。
- Knowledge contradiction:
  -古い item を削除せず superseded / retracted とする。
- Context resolver partial failure:
  - missing scope / stale worker / unresolved content ref を隠さない。

## 15. Storage

O1 の初期実装は Temote owner-only state directory の SQLite を候補とする。

ただし contract は storage backend に固定しない。

初期 authority:

- Task / Execution: existing Temote/backend stores
- Observation: Temote observation store
- Knowledge: derived local store

複数 host の global knowledge authority / replication は初期 scope 外。
remote head は Temote の authenticated frontend 経由で同じ owning host の Context Resolver にアクセスする。

multi-host aggregation を必要とする場合は Gateway の ownership/routing contract と整合した別 packet にする。

## 16. Public surface

最初から多数の MCP tools を増やさない。

優先 surface:

```text
context_resolve
context_status
```

debug / owner-only CLI:

```text
temote observation list ...
temote knowledge list ...
temote knowledge inspect ...
temote memory worker status
temote memory worker run
```

raw observation dump を通常の remote MCP surface に出さない。

## 17. Explicit non-goals

初期 scope に含めない:

- autonomous task planning
- backend/model auto routing
- generic workflow engine
- agent inbox / mail system
- social multi-agent chat
- generic RAG platform
- vector database requirement
- LoRA / adapter routing
- hidden chain-of-thought capture
- Temote 外の全 chat transcript の収集
- worker による task state の直接変更

## 18. Implementation packets

### O0 — contract (this document)

- [x] observation boundary
- [x] raw vs derived authority
- [x] security / content reference policy
- [x] worker responsibility
- [x] context resolver responsibility
- [x] initial non-goals

### O1 — observation journal

Prerequisite: Phase A の common task identity / typed request boundary。

- [ ] `Observation` schema + schema version
- [ ] owner-only store
- [ ] common orchestration entry/exit の recorder
- [ ] task/control instruction references
- [ ] execution/evidence/verification/delivery observation hooks
- [ ] idempotent append key
- [ ] gap/backfill/reconcile behavior
- [ ] no secret-bearing structured fields test

Acceptance:

- 同じ operation の retry で observation が重複増殖しない。
- MCP/local の同じ semantic operation が同じ observation shape になる。
- backend start が成功し observation response が消失しても backend を二重起動しない。
- raw content を無制限に tool response へ inline しない。

### O2 — Context Resolver without LLM memory

Prerequisite: O1。

- [ ] task / execution / workspace / verification current state projection
- [ ] repository/task scoped recent instruction lookup
- [ ] deterministic context bundle
- [ ] freshness / partial state
- [ ] `context_resolve` contract

ここまでで head switch の最低価値を成立させる。
Memory Worker が未実装でも、過去の instruction と verified state を次の head が取得できる。

### O3 — Memory Worker

Prerequisite: O1 + O2。

- [ ] worker checkpoint
- [ ] batch read
- [ ] knowledge extraction schema
- [ ] support refs required
- [ ] dedupe / supersession
- [ ] stale worker reporting
- [ ] retry idempotency
- [ ] worker failure が task state を変更しない

### O4 — Knowledge-aware resolver

Prerequisite: O3。

- [ ] current knowledge selection
- [ ] task-specific vs repository-wide scope policy
- [ ] unresolved / failure pattern retrieval
- [ ] budgeted context bundle
- [ ] provenance included
- [ ] stale / superseded knowledge exclusion by default

## 19. Priority and dependency

この track は **high priority**。

実装順は Temote restructure 全体では次を推奨する。

```text
A1/A2/A3  common orchestration + identity
       |
       +------> O1 observation journal
       |            |
       |            +--> O2 deterministic context
       |            |         |
       |            |         +--> O3 worker --> O4 knowledge context
       |            |
       +------> B local frontend
       |
       +------> F/C workspace foundation
```

O1/O2 を D (environment) / E (delivery) より優先する。
理由は、後から proxy logging を付けると operation / evidence / state transition の捕捉点を再度変更する必要があるため。

ただし O1 のために A の common identity を飛ばして backend module ごとに個別 logger を追加しない。

## 20. Acceptance criteria

- [ ] head が変わっても repository/task の relevant context を Temote から取得できる
- [ ] coding agent に memory maintenance prompt / tool call を要求しない
- [ ] 「誰/何が、どの backend に、どの instruction を出したか」を authorized scope 内で追跡できる
- [ ] caller/agent claim と Temote verified execution state を区別する
- [ ] raw observation は worker output から独立して保持される
- [ ] derived knowledge は support refs を持ち、再生成可能
- [ ] superseded/stale knowledge を current fact として返さない
- [ ] observation/worker failure が accepted backend operation の盲目的 replay を起こさない
- [ ] MCP/local/HTTP/Gateway で semantic observation contract が変わらない
- [ ] secrets を observation metadata / ordinary output に複製しない
- [ ] raw transcript dump を通常の public surface にしない
- [ ] worker failure 時も task execution は独立して継続できる

## 21. Principle

> Temote が仕事をする agent に「覚えておけ」と頼むのではなく、Temote 自身が observable execution を構造化して記録し、別 worker が後から理解する。次の head は、その整理済み context と根拠を受け取って続行する。

これにより Temote は planner や memory agent にならず、head-independent な execution substrate と context continuity を提供する。
