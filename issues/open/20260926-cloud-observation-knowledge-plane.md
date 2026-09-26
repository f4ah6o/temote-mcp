# O3C: Temote Fabric shared observation / knowledge plane

Status: implementation in progress / C0 complete  
Repository: `f4ah6o/temote-mcp`  
Parent: `issues/open/20260925-observation-context-memory-plane.md`  
Umbrella: `issues/open/20260924-temote-development-harness-restructure.md`  
Naming / boundary: `issues/open/20260926-temote-fabric-product-boundary.md`  
Priority: high — O1/O2 local continuity is already useful; this packet makes O3/O4 head- and host-independent  
Created: 2026-09-26 (Asia/Tokyo)

## 1. Decision

Observation / context / memory の shared plane は **Temote Fabric** が持つ。

初期実装は既存 Cloudflare Gateway deployment (`gateway/`) を拡張して Fabric へ移行する。Fabric を execution authority にはしない。

- Temote host:
  - Task / Execution / Workspace / Evidence の authority
  - O1 local observation journal の writer
  - Cloudflare が落ちても coding task を継続する
- Fabric Worker (current Gateway Worker):
  - authenticated observation ingest
  - shared context read surface
  - O3 async worker の enqueue / consumer entry point
- D1:
  - sanitized structured observation replica
  - worker checkpoint / run state
  - derived knowledge / support / supersession
- R2:
  - large content / evidence の **optional** object store
  - database の代替にはしない
- Queue:
  - O3 Memory Worker の非同期 trigger / retry
- existing `GatewaySession` / `GatewayRegistry` Durable Objects:
  - host/session routing のまま
  - knowledge database として流用しない

初期実装では現行 `temote-mcp-gateway` Worker deployment に bindings と modules を追加し、`issues/open/20260926-temote-fabric-product-boundary.md` の migration packet に従って `temote-fabric` へ移行する。
将来 worker を別 deployment に分離しても D1 schema / replication contract は変えない。

## 2. Why this changes the previous O0 storage assumption

O0 は initially:

- Observation: owning host local store
- Knowledge: derived local store
- multi-host global knowledge authority: initial scope 外

としていた。

しかし Temote の product goal は、caller/head だけでなく execution host を替えても、

- ChatGPT
- local coordinator
- Codex
- OpenCode
- Devin
- future coordinator

が同じ repository knowledge を利用できることにある。

Knowledge を host-local SQLite に置くと、head independence は得られても host independence が得られない。
したがって O3/O4 では cloud shared projection を first-class にする。

これは local state を捨てる変更ではない。
local observation journal は Cloudflare への durable spool / recovery source として残す。

## 3. Product boundary

### 3.1 Temote host remains authoritative

Cloudflare 側は以下を authority にしない。

- backend process が本当に running か
- task が completed / failed か
- workspace の current filesystem state
- VCS working state
- approval / permission decision
- verification が実際に実行されたか

これらの authority は Temote host / backend state に残る。

Cloud D1 にある execution state は **replicated observed state** であり、host の authoritative state を上書きしない。

### 3.2 Cloud plane responsibilities

Cloud plane は以下を担当する。

1. sanitized observation の multi-host 集約
2. repository/task scope の durable indexing
3. O3 Memory Worker
4. derived knowledge の保存
5. support / supersession / worker freshness
6. host が offline でも利用できる Context Resolver
7. head / transport に依存しない authenticated retrieval

### 3.3 Fabric is the connective plane, not the execution brain

Fabric に planner / autonomous workflow / task routing intelligence を追加しない。

Fabric が行うのは:

- authenticate
- ingest
- route
- queue
- project
- resolve

まで。

Memory Worker が生成した knowledge から coding task を勝手に起動しない。

## 4. Target architecture

```text
              head / caller
     ChatGPT / Codex / OpenCode / human
                    |
                    v
          Cloudflare Access / MCP
                    |
                    v
        +--------------------------+
        | Temote Fabric Worker     |
        |                          |
        | MCP routing              |
        | context_resolve/status   |
        | observation ingest       |
        +----+---------------+-----+
             |               |
             |               +-------------------+
             |                                   |
             v                                   v
   GatewaySession / Registry                     D1
       Durable Objects                  observations / knowledge
             ^                                   ^
             |                                   |
             |                            +------+------+
             |                            | Memory Worker|
             |                            | Queue consumer
             |                            +------+------+
             |                                   ^
             |                                   |
             |                                Queue
             |                                   ^
             |                                   |
             +------------------+----------------+
                                |
                    authenticated host channel
                                |
                 +--------------+--------------+
                 |                             |
                 v                             v
            Temote host A                 Temote host B
            local O1 JSONL                local O1 JSONL
            execution authority           execution authority
                 |
                 +--> optional large/safe content ----> R2
```

## 5. Authority and durability model

### 5.1 Local observation journal

Existing O1 journal remains:

```text
<state_dir>/temote-mcp/observations/
  obs-<session>.jsonl
  obs-<session>.meta.json
```

Responsibilities:

- append before / independently from cloud replication
- survive temporary Fabric / D1 / Queue outage
- preserve source revision and gap metadata
- provide local `context_resolve` fallback
- act as replication spool

Cloud sync failure MUST NOT turn a successful/active coding task into failed.

### 5.2 D1 observation replica

D1 stores the structured, sanitized envelope needed for:

- cross-host indexing
- worker extraction
- provenance
- support references
- freshness / gaps

D1 is the shared cloud replica, not the original execution authority.

### 5.3 Derived knowledge

Knowledge is always reproducible projection.

A worker/model change may rebuild knowledge from retained observations without changing Task / Execution authority.

## 6. D1 data model

Exact migration SQL is implementation work, but the contract is fixed around the following tables.

### 6.1 `observation_sources`

One row per owner + host + session source.

Minimum fields:

```text
owner_id
host_id
session_id
repository_key
source_base_revision
source_head_revision
acked_through_revision
cloud_head_seq
journal_degraded
gap_count
last_synced_at
```

Purpose:

- replication cursor
- source completeness
- local compaction gap detection
- cloud freshness reporting

### 6.2 `observations`

Minimum fields:

```text
cloud_seq INTEGER PRIMARY KEY
owner_id
host_id
session_id
observation_id
source_revision
schema_version
repository_key
workspace_id
task_id
execution_id
operation_id
kind
action
actor_transport
actor_principal_ref
target_backend
content_kind
content_preview
content_digest
content_ref
state_status
state_revision
evidence_refs
observed_at
ingested_at
```

Required uniqueness:

```text
UNIQUE(owner_id, host_id, session_id, observation_id)
UNIQUE(owner_id, host_id, session_id, source_revision)
```

`cloud_seq` is the worker-side monotonic cursor.
It does not replace the source journal revision.

### 6.3 `memory_checkpoints`

```text
owner_id
repository_key
worker_id
producer_version
last_cloud_seq
last_success_at
last_error_at
last_error_code
stale
```

A checkpoint advances only after the corresponding knowledge writes commit.

### 6.4 `memory_runs`

Stable run identity:

```text
run_id = hash(owner_id, repository_key, producer_version, from_seq, to_seq)
```

Fields include:

```text
run_id
repository_key
producer_version
from_seq
to_seq
status
attempt_count
started_at
completed_at
error_code
```

Duplicate Queue delivery first checks this row.
A completed run is a no-op.

### 6.5 `knowledge_items`

Use the O0 knowledge contract and add cloud projection metadata.

```text
knowledge_id
owner_id
repository_key
scope_type
scope_id
kind
semantic_key
text
status
confidence
valid_from
valid_until
producer
producer_version
produced_at
source_through_cloud_seq
```

Scope identity rules:

- `scope_id` is required and non-empty
- user scope uses `scope_id = owner_id`
- repository scope uses `scope_id = repository_key`
- workspace/task/execution scopes use their stable non-empty IDs
- semantic dedupe therefore cannot be bypassed by SQLite `NULL` uniqueness semantics

Statuses remain:

- candidate
- supported
- current
- superseded
- retracted

### 6.6 `knowledge_support`

```text
owner_id
repository_key
knowledge_id
observation_cloud_seq
observation_id
support_role
```

Every supported/current fact, decision, constraint, unresolved item and failure pattern must have at least one support row.

Support rows are namespace-bound with composite foreign keys. The `observation_cloud_seq` + `observation_id` pair must identify the same observation in the same owner/repository namespace; cross-owner or cross-repository provenance links are invalid.

### 6.7 `knowledge_supersession`

```text
owner_id
repository_key
new_knowledge_id
old_knowledge_id
relationship
created_at
```

Old rows are not deleted when superseded. Supersession edges use composite owner/repository foreign keys for both knowledge IDs, so an edge cannot cross tenant or repository namespaces.

## 7. Repository identity

Cloud knowledge MUST NOT key by local cwd.

Cross-host scope requires a stable `repository_key` from the VCS/repository identity contract.

Preferred forms:

1. canonical forge identity, e.g. `github:f4ah6o/temote-mcp`
2. backend-neutral stable repository identity from Phase F/V
3. explicit non-forge repository fingerprint

Rules:

- local path is metadata only
- repository A knowledge is never implicitly visible to repository B
- unresolved repository identity may sync session/task observations, but MUST NOT promote them to repository-wide current knowledge
- repository rebind/rename needs an explicit alias/migration record, not string guessing

## 8. Host -> Fabric replication protocol

Add a host-authenticated endpoint under the existing host channel.

Conceptual endpoint:

```text
POST /v1/hosts/<host_id>/observations/sync
```

The current Gateway implementation's per-host bearer token + Access/service-token boundary is reused as the initial Fabric Link authentication boundary.
Do not create a second weaker host credential system.

Request shape:

```json
{
  "schema_version": 1,
  "session_id": "session-a",
  "source_base_revision": 101,
  "source_head_revision": 160,
  "journal_degraded": false,
  "gap_count": 0,
  "records": [
    {
      "source_revision": 121,
      "observation": {}
    }
  ]
}
```

Response shape:

```json
{
  "session_id": "session-a",
  "acked_through_revision": 140,
  "cloud_head_seq": 9321,
  "complete": true
}
```

### 8.1 Replication rules

1. local append happens first
2. gateway-agent reads after its last acknowledged source revision
3. Fabric validates host/session ownership and schema
4. structured secret-bearing fields are rejected / absent by contract
5. D1 insert is idempotent
6. ack advances only over a contiguous accepted source range
7. host persists its sync cursor only after ack
8. retry sends the same records; duplicates do not create new observations
9. ingestion success may enqueue repository memory work
10. Queue / worker failure does not roll back observation ingest

### 8.2 Local compaction / gaps

If cloud cursor falls behind local `base_revision` because the local bounded journal compacted before sync:

- do not fabricate missing observations
- update `observation_sources.journal_degraded = true`
- record the missing revision interval / gap count
- expose partial/stale status through `context_status`
- continue syncing later available records

Context Resolver must never claim complete provenance across a known gap.

## 9. O3 Memory Worker

### 9.1 Trigger

After an ingest commits new observations for a repository, Fabric sends a small Queue message.

Queue payload is only a wake-up/high-watermark hint:

```json
{
  "owner_id": "...",
  "repository_key": "github:f4ah6o/temote-mcp",
  "through_cloud_seq": 9321
}
```

Do not put observation bodies in Queue messages.

### 9.2 Consumer

The Worker Queue consumer:

1. reads `memory_checkpoints`
2. selects observations `cloud_seq > last_cloud_seq` for the repository
3. bounds one extraction batch
4. creates stable `memory_runs.run_id`
5. groups related observations into episodes
6. calls a provider-neutral extractor adapter
7. validates worker output
8. writes knowledge/support/supersession + checkpoint as one committed unit
9. marks the run completed
10. acknowledges Queue delivery

Cloudflare Queues may redeliver; O3 is designed for at-least-once execution.

### 9.3 Extractor is replaceable

Do not couple D1 schema to one model/provider.

Conceptual adapter:

```text
extract_knowledge(
  existing_current_knowledge,
  observations,
  scope_policy,
  output_schema
) -> candidates
```

The provider may initially be a cheap hosted model.
Provider configuration is a Worker secret/configuration concern, not Temote core semantics.

### 9.4 Retry idempotency

A repeated run with the same stable run id:

- if completed: no-op
- if incomplete before DB commit: rerun safely
- if model call succeeded but DB commit failed: rerun is allowed
- never duplicate a completed run's projection solely due to Queue redelivery

Knowledge writes use semantic keys + support references; worker output cannot directly mutate Task / Execution state.

### 9.5 Concurrency

Initial correctness rule: only one active projection update per repository scope.

Implementation choices, in priority order:

1. D1 run/checkpoint compare-and-commit where sufficient
2. repository-scoped Durable Object coordinator if concurrent workers create contention
3. do not serialize all repositories through existing `GatewayRegistry` or `GatewaySession`

A new coordinator DO is optional, not an O3 prerequisite unless tests demonstrate a race that D1 run/checkpoint semantics cannot prevent.

## 10. R2 policy

R2 is **not** the primary O3 database.

Use R2 only for payloads that are:

- too large / unsuitable for D1 rows
- explicitly cloud-sync eligible
- useful to resolve a support reference later

Examples:

```text
large normalized evidence
explicitly retained task content
compressed observation archive
model input/output diagnostic artifact with safe retention policy
```

Default O3 does NOT upload:

- full chat transcripts
- hidden reasoning
- arbitrary workspace files
- secrets / tokens / credentials
- every stdout/stderr blob

### 10.1 Content tiers

```text
Tier 0: structural metadata only     -> D1
Tier 1: bounded safe preview/digest  -> D1
Tier 2: large explicitly-safe body   -> R2 + D1 ref
Tier L: local-only content           -> local ref; cloud resolver marks unresolved if host unavailable
```

This keeps the existing reference-first safety invariant.

## 11. Context Resolver behavior

### 11.1 Fabric path

When cloud bindings are enabled, Fabric should handle `context_resolve` / `context_status` as cloud-aware operations rather than blindly proxying every request to one session host.

Backward compatibility:

- existing session-scoped request:
  - resolve repository/source mapping from cloud state
  - if cloud mapping is unavailable, proxy to owning host as today
- repository-scoped request:
  - resolve directly from D1
  - does not require an online execution host

### 11.2 Result authority

A context result distinguishes:

```text
authoritative/live:
  only when fetched from an owning host in the request path

replicated_observed:
  latest state known in D1

derived:
  worker-produced knowledge
```

Derived knowledge never overwrites an authoritative/live state field.

### 11.3 Freshness

Return at least:

```text
latest_cloud_seq
worker_last_cloud_seq
worker_lag
worker_last_success_at
source_head_revision
source_acked_revision
source_gap_count
cloud_observation_stale
knowledge_stale
partial
```

If a requested minimum source/cloud revision is not present, return stale/partial instead of silently serving it as current.

### 11.4 Offline acceptance

A remote head must be able to obtain repository knowledge while all execution hosts are offline, subject to the last successfully synced revision.

That is the main reason to move O3/O4 projection into the cloud plane.

## 12. Security and tenancy

### 12.1 Auth reuse

Reuse the current Gateway implementation's authentication boundaries as the initial Fabric boundaries:

- caller: Cloudflare Access / MCP auth
- host: per-host bearer token + existing host identity checks

Observation sync is host-only.
Raw observation ingest is never exposed as a public MCP tool.

### 12.2 Owner namespace

Even if the first deployment is single-user/single-owner, every cloud table carries an `owner_id` namespace.

Do not rely on repository name alone as a security boundary.

### 12.3 Secret handling

Before sync:

- structured credential fields are excluded locally
- observation schema is allow-listed
- Fabric rejects unknown oversized/sensitive structured fields
- logs contain bounded identifiers, not content bodies

Free text cannot be perfectly secret-scanned.
Therefore large/full content cloud upload remains opt-in and policy-controlled.

## 13. Fabric code boundary

Do not grow the current `gateway/src/index.js` into one memory monolith. Split responsibilities first; the source-tree rename to `fabric/` is tracked separately in the Fabric naming/migration issue.

Target modules:

```text
gateway/src/
  index.js
  protocol.js
  observation/
    ingest.js
    schema.js
    store.js
    resolver.js
  memory/
    consumer.js
    extractor.js
    projection.js
```

Conceptual bindings:

```toml
[[d1_databases]]
binding = "OBSERVATION_DB"
database_name = "temote-observation"
database_id = "..."

[[queues.producers]]
binding = "MEMORY_QUEUE"
queue = "temote-memory"

[[queues.consumers]]
queue = "temote-memory"
max_batch_size = 10
max_batch_timeout = 5

# Add only when Tier 2 content is implemented.
[[r2_buckets]]
binding = "OBSERVATION_CONTENT"
bucket_name = "temote-observation-content"
```

The current implementation still uses:

```text
GATEWAY_SESSIONS
GATEWAY_REGISTRY
```

These Durable Object bindings remain routing/liveness state only until the non-destructive Fabric naming migration. Target names are `FABRIC_SESSIONS` / `FABRIC_REGISTRY`; renaming deployed DO classes/bindings requires the migration checks in the Fabric naming issue.

## 14. Failure semantics

### Fabric/D1 unavailable

- local O1 append succeeds
- coding/execution continues
- sync cursor does not advance
- gateway-agent retries later
- no backend operation replay

### D1 ingest succeeds / Queue send fails

- observation remains durable in D1
- source ack may succeed because ingest succeeded
- mark/enqueue repair using a later ingest, scheduled repair, or explicit worker sweep
- worker lag is visible

Queue is a wake-up mechanism, not the sole record of pending work.

### Queue redelivery

- stable run/checkpoint makes processing idempotent
- duplicate knowledge must not multiply

### Memory provider unavailable

- observations remain synced
- checkpoint does not advance past uncommitted work
- knowledge becomes stale
- Task / Execution state is unchanged

### R2 unavailable

- Tier 0/1 ingestion may continue
- Tier 2 content ref remains unresolved/pending
- do not fail unrelated task execution

### Host offline

- cloud context remains readable through last synced revision
- result says replicated/stale as appropriate

### Local journal gap

- cloud source becomes degraded/partial
- never infer missing content
- future records may still sync

## 15. Retention and rebuild

Initial policy:

- D1 keeps structured observation envelopes required for provenance/rebuild
- Knowledge remains a rebuildable projection
- R2 retention is separately configurable because its payload may be larger/more sensitive
- deleting derived knowledge does not delete observations
- deleting observations requires marking dependent knowledge support as unavailable/retracted or retaining an archive reference

Do not implement automatic observation pruning until rebuild/provenance behavior for archived records is tested.

## 16. Migration plan

### C0 — contract / schema

- [x] add this child design to O0 / umbrella links
- [x] define D1 migration files
- [x] define cloud observation schema version
- [x] define owner/repository identity mapping
- [x] define sync request/ack contract
- [x] define freshness/degraded result contract

C0 implementation:
- `gateway/migrations/0001_observation_knowledge.sql`
- `gateway/src/observation/schema.js`
- `gateway/test/cloud-observation-schema.test.mjs`
- `gateway/test/cloud_observation_schema_sqlite.py` (SQLite constraint acceptance)

C0 intentionally does not add the D1 binding, observation ingest endpoint, Queue binding, or R2 binding. Those remain C1/C4/C6 work.

### C1 — Fabric D1 observation ingest

- [ ] add D1 binding
- [ ] add `observation_sources` / `observations`
- [ ] host-authenticated sync endpoint
- [ ] idempotent batch ingest
- [ ] contiguous ack
- [ ] gap/degraded tracking
- [ ] no secret-bearing structured field tests

### C2 — host replicator

- [ ] gateway-agent reads local O1 journal
- [ ] durable per-session ack cursor
- [ ] bounded batches
- [ ] retry without duplicate insert
- [ ] compaction-gap reporting
- [ ] Fabric offline does not fail task execution

### C3 — Fabric Context Resolver before LLM memory

- [ ] D1 deterministic repository/task projection
- [ ] cloud `context_status`
- [ ] Fabric `context_resolve` read path
- [ ] offline-host acceptance
- [ ] legacy session fallback
- [ ] freshness / partial / authority labels

This stage gives multi-host deterministic continuity before O3 model extraction.

### C4 — O3 Queue Memory Worker

- [ ] Queue producer/consumer binding
- [ ] `memory_runs` / `memory_checkpoints`
- [ ] provider-neutral extractor adapter
- [ ] output schema validation
- [ ] support refs mandatory
- [ ] dedupe / semantic key
- [ ] supersession
- [ ] stable-run retry idempotency
- [ ] worker failure does not alter Task / Execution

### C5 — O4 knowledge-aware cloud resolver

- [ ] current fact / decision / constraint selection
- [ ] unresolved / failure pattern retrieval
- [ ] stale/superseded exclusion by default
- [ ] provenance/support included
- [ ] budgeted context
- [ ] worker lag surfaced

### C6 — optional R2 Tier 2

Implement only after C1-C5 works without it.

- [ ] R2 binding
- [ ] explicit cloud-eligible content contract
- [ ] bounded upload policy
- [ ] D1 content refs
- [ ] retention/deletion tests
- [ ] no transcript-dump regression

## 17. Tests / acceptance

### Fabric tests

- [ ] same observation batch twice -> one D1 observation set
- [ ] out-of-order/gapped source revision does not advance contiguous ack incorrectly
- [ ] wrong host token cannot write another host's source
- [ ] unknown owner/repository cannot read another scope
- [ ] content body does not appear in ordinary gateway logs
- [ ] D1 ingest failure does not fabricate ack
- [ ] Queue send failure after D1 commit leaves recoverable worker lag

### Worker tests

- [ ] same Queue message twice -> one completed `memory_run`
- [ ] completed checkpoint is not advanced on failed projection
- [ ] every current/supported item has support refs
- [ ] task-scoped temporary state is not promoted repository-wide without policy
- [ ] superseded item is excluded from current context
- [ ] provider failure leaves execution state untouched

### End-to-end

1. Host A performs task A and records O1 observations.
2. gateway-agent syncs them.
3. Host A goes offline.
4. O3 produces repository knowledge.
5. A different head calls Fabric `context_resolve`.
6. It receives:
   - task A instruction reference
   - replicated final observed state
   - current derived decisions/facts
   - unresolved items
   - support refs
   - source/worker freshness
7. No memory-maintenance prompt/tool call was sent to the coding backend.

Then:

8. Host B performs task B in the same repository identity.
9. Host B observations join the same repository cloud scope.
10. Context resolution contains both hosts' supported history without mixing another repository.

## 18. Explicit non-goals

This packet does not add:

- Cloudflare as Task/Execution authority
- autonomous planning
- model/backend auto-routing
- task creation from memories
- arbitrary transcript collection
- vector DB requirement
- embeddings requirement
- R2 as a relational/query database
- reuse of Gateway routing DOs as the memory database
- mandatory cloud connectivity for local Temote execution

## 19. Principle

> Execute locally, observe durably, replicate safely, understand globally.

Temote Fabric は「仕事をする場所」ではなく、host-independent observation / context continuity の authenticated shared substrate になる。
Execution authority は host に残し、D1 knowledge は provenance 付きの rebuildable projection とする。
