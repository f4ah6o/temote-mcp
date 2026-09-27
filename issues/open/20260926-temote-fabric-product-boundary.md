# Temote Fabric: product name / responsibility boundary / gateway migration

Status: implementation in progress / FBR1 complete  
Repository: `f4ah6o/temote-mcp`  
Umbrella: `issues/open/20260924-temote-development-harness-restructure.md`  
Related:
- `issues/open/20260925-observation-context-memory-plane.md`
- `issues/open/20260926-cloud-observation-knowledge-plane.md`
Created: 2026-09-26 (Asia/Tokyo)

## 1. Naming decision

The shared remote substrate currently implemented under `gateway/` is named **Temote Fabric**.

The name extends the product idea of **Temote = 手元 + remote**.

Temote should let a caller use work happening "at hand" and remotely without changing the execution model merely because the caller or executor moved.

Temote Fabric is the connective side of that idea:

```text
Temote
  手元 × remote
  "where the caller/executor is should not change the work model"

Temote Fabric
  local × remote × hosts × heads × context
  "where the work happened should not break routing or continuity"
```

Fabric is not a new execution authority.
It is the authenticated shared substrate connecting Temote Hosts and callers.

## 2. Canonical product vocabulary

### Temote

The overall product / CLI / development harness.

Responsibilities include:

- task / execution orchestration
- workspace / VCS integration
- backend lifecycle
- verification / delivery
- local and remote frontends

### Temote Host

A machine/runtime that owns execution.

Examples:

- macOS development machine
- Linux workstation/server
- Temote inside WSL2
- future native Windows host

A Host is authoritative for:

- live backend process state
- Task / Execution lifecycle
- workspace state
- permissions / approvals
- local evidence and verification
- local O1 observation journal

"Host" is preferred over "node" in user-facing vocabulary because it describes where work runs without implying a distributed-compute cluster.

### Temote Fabric

The shared connective plane between callers and Hosts.

Responsibilities:

- authenticated remote entry
- host discovery / routing
- host/session connectivity
- observation replication
- shared context
- derived memory / knowledge
- host-offline context reads up to the last synced revision

Fabric does **not** own:

- filesystem truth
- live backend truth
- approval decisions
- autonomous planning
- task execution
- automatic backend/model choice

### Fabric Link

A logical authenticated connection from one Temote Host to Temote Fabric.

This is a protocol/lifecycle term, not necessarily a standalone daemon name.

A link has:

- `host_id`
- host credential
- connection generation / instance identity
- lease / liveness
- observation replication cursor
- capability/protocol version

The current host-level `gateway-agent` implements most of what becomes the Fabric Link runtime.

### Fabric Context

The read projection returned across hosts/heads.

It combines, with explicit authority labels:

- replicated observations
- current derived knowledge
- unresolved items
- support references
- freshness / gaps
- optional live Host state when available

### Fabric Memory

The O3/O4 derived knowledge subsystem inside Fabric.

It is a rebuildable projection, never execution authority.

### Fabric Observation

The shared sanitized observation replica inside Fabric.

The canonical raw local O1 journal still exists on each Host.

## 3. Product model

```text
                         caller / head
               ChatGPT / Codex / OpenCode / human
                              |
                              v
                    +-------------------+
                    |   Temote Fabric   |
                    |                   |
                    | access / routing  |
                    | observation       |
                    | context / memory  |
                    +----+---------+----+
                         |         |
                 Fabric Link   Fabric Link
                         |         |
                         v         v
                  Temote Host A  Temote Host B
                  execution      execution
                  workspace      workspace
                  local journal  local journal
```

The user-facing story is intentionally simple:

> Temote runs work at hand or remotely. Temote Fabric connects those places and carries the context between them.

## 4. Why "Fabric", not "Gateway"

`gateway` describes only the original ingress/routing responsibility.

The current/future shared service also carries:

- host registry
- remote routing
- observation synchronization
- repository-wide context
- memory / knowledge projection
- offline read continuity

Calling all of this "Gateway" makes the architecture appear to be one large proxy and encourages unrelated responsibilities to accumulate in one routing module.

"Fabric" describes a substrate connecting multiple independent execution authorities without implying that the center executes their work.

## 5. Architectural boundary

The naming change must reinforce the architecture rather than hide it.

### 5.1 Execution plane

Owned by Temote Hosts.

```text
Task
Execution
Workspace
VCS
Verification
Delivery
Approval
Backend lifecycle
```

### 5.2 Fabric plane

Cloud/shared responsibilities.

```text
Access
Registry
Routing
Link lifecycle
Observation replication
Context resolution
Memory projection
Shared provenance
```

### 5.3 Rule

A Fabric component may report or project execution state, but it must not silently become the authority for that state.

For example:

```text
Host says execution=running
    -> authoritative/live

D1 last observed execution=running
    -> replicated_observed

Memory says "task appears complete"
    -> derived claim; never execution state
```

## 6. Cloudflare implementation mapping

"Fabric" is a product/domain name, not a hard dependency on Cloudflare.

Current implementation target:

```text
Temote Fabric
├─ Cloudflare Worker
│  ├─ MCP / remote access
│  ├─ host APIs
│  ├─ observation ingest
│  └─ context resolver
├─ Durable Objects
│  ├─ FabricSession     (current GatewaySession)
│  └─ FabricRegistry    (current GatewayRegistry)
├─ D1
│  ├─ observations
│  ├─ source cursors
│  ├─ memory runs/checkpoints
│  └─ knowledge/support/supersession
├─ Queues
│  └─ asynchronous memory work
└─ R2 (optional)
   └─ explicitly-safe large content
```

A future non-Cloudflare implementation should be able to implement the same Fabric contract.

## 7. Source tree direction

Current:

```text
gateway/
  src/
  contract/
  test/
  wrangler.toml
```

Target:

```text
fabric/
  src/
    access/
    routing/
    link/
    observation/
    context/
    memory/
  contract/
  test/
  wrangler.toml
```

Do not perform a blind directory rename before contract compatibility and deployment migration are defined.

During migration, documentation may say "Fabric (currently implemented under `gateway/`)" to distinguish product terminology from the current path.

## 8. Runtime / deployment naming

Current -> target:

```text
temote-mcp-gateway           -> temote-fabric
GatewaySession               -> FabricSession
GatewayRegistry              -> FabricRegistry
GATEWAY_SESSIONS             -> FABRIC_SESSIONS
GATEWAY_REGISTRY             -> FABRIC_REGISTRY
GATEWAY_DEPLOYMENT           -> FABRIC_DEPLOYMENT
TEMOTE_MCP_GATEWAY_*         -> TEMOTE_FABRIC_*
gateway contract             -> Fabric remote contract
gateway health identity      -> temote-fabric
```

The exact environment-prefix migration must support a compatibility period.
Do not make existing deployed Hosts simultaneously lose their endpoint and credentials.

## 9. Host-side UX

The current command:

```sh
temote-mcp gateway-agent --host-id mac-main
```

is an implementation-shaped name.

Target user-facing model:

```sh
temote host
temote fabric connect
temote fabric status
```

Preferred steady state:

- `temote host` owns local supervision/execution.
- If Fabric configuration exists, the Host may maintain its Fabric Link automatically.
- `temote fabric connect` is available for explicit/manual lifecycle control and diagnostics.
- Users should not normally need to understand a permanent separate "agent" process just to connect a Host.

Compatibility path:

```text
temote-mcp gateway-agent
    -> temporary compatibility command
    -> same Fabric Link implementation
    -> warning / migration guidance only after new command is proven
```

Do not remove the old command in the same packet that introduces the new path.

## 10. Remote UX

Fabric remains one remote entry point.

Conceptual public surface:

```text
host_list
host_info
session_list
session_start
...

context_resolve
context_status
```

Do not rename stable MCP tools just to make them metaphorical.

The product name can be Fabric while API names remain literal and descriptive.

For example, prefer:

```text
context_resolve
observation sync
host registry
```

over metaphor-heavy names such as:

```text
weave
thread
loom
strand
```

The textile metaphor is explanatory branding, not an API vocabulary requirement.

## 11. "Spool" terminology

The local O1 journal naturally behaves like a durable spool before cloud replication.

This analogy is useful in architecture explanations:

```text
local observation spool -> Fabric -> shared context
```

But keep implementation/API names literal:

- observation journal
- replication cursor
- sync batch
- D1 observation replica

Do not rename core objects to `Spool`, `Loom`, etc.

## 12. Fabric identity and repository continuity

Fabric's value is not only network connectivity.

A repository may be worked on from different Hosts:

```text
Mac Host
  github:f4ah6o/temote-mcp
        |
        v
     Fabric
        ^
        |
Linux Host
  github:f4ah6o/temote-mcp
```

Both map to the same stable repository identity while preserving distinct:

- host IDs
- session IDs
- workspace IDs
- task/execution IDs
- source observation revisions

This enables context continuity without pretending the two local workspaces are identical.

## 13. Failure model implied by the name

Fabric should degrade like connective infrastructure, not like execution authority.

If Fabric is unavailable:

```text
Temote Host execution   -> continues
local observation       -> continues
Fabric sync             -> pending
remote routing          -> unavailable/degraded
shared context freshness-> stale
```

When Fabric returns:

```text
local observation journal
  -> idempotent replication
  -> D1
  -> memory catch-up
  -> context freshness restored
```

No accepted backend operation is automatically replayed just because the Fabric connection disappeared.

## 14. Deployment migration requirements

A rename of a live remote service is not a cosmetic filesystem change.

The migration packet must cover:

1. Worker service name
2. routes/custom domain
3. Cloudflare Access application/audience
4. Durable Object class rename/migration rules
5. DO bindings
6. D1/Queue/R2 bindings
7. secrets
8. host env variables
9. health identity
10. MCP serverInfo name/title
11. contract fingerprint generation paths
12. CI/deploy scripts
13. docs
14. rollback

### 14.1 Durable Object caution

Do not simply rename deployed DO classes in Wrangler and assume stored state follows.

The implementation packet must verify Cloudflare's class migration requirements and preserve active host/session routing state or provide an explicit reconnect-compatible migration.

The product naming design does not authorize destructive DO storage recreation.

### 14.2 Endpoint compatibility

Preferred migration:

```text
old gateway hostname/route
        |
        +---- compatibility route ----> new Fabric Worker revision

new Fabric hostname/route
        |
        +-----------------------------> same Fabric Worker revision
```

when practical.

Hosts can then migrate endpoint/config independently before the old route is retired.

Exact routing depends on the current deployed configuration and must be measured before implementation.

## 15. Code structure boundary

The new name is specifically intended to stop `index.js` from becoming "everything remote".

Target responsibilities:

```text
fabric/src/index.js
  composition / Worker entrypoint only

fabric/src/access/
  caller + host auth

fabric/src/routing/
  host/session registry and dispatch

fabric/src/link/
  host connection protocol / lease / generation

fabric/src/observation/
  sync contract / ingest / D1 observation replica

fabric/src/context/
  deterministic + knowledge-aware resolver

fabric/src/memory/
  Queue consumer / extractor / projection
```

Routing DOs and memory projection remain separate state domains even though they deploy together initially.

## 16. Migration packets

### FBR0 — terminology and contract

- [x] choose Temote Fabric
- [x] define Host / Fabric / Fabric Link / Context / Memory vocabulary
- [x] define execution-vs-Fabric authority boundary
- [x] update relevant design docs to canonical Fabric terminology
- [x] record compatibility names for current gateway implementation

### FBR1 — internal module split before rename

- [x] split current `gateway/src/index.js` by routing/access responsibilities
- [x] add observation/context modules without mixing them into DO routing classes
- [x] preserve public contract fingerprint
- [x] Node tests unchanged/green

This may still live under `gateway/`.

### FBR2 — user-facing command model

- [ ] introduce `temote fabric status`
- [ ] introduce `temote fabric connect` or automatic Host link lifecycle
- [ ] keep `gateway-agent` compatibility path
- [ ] migration diagnostics for old env vars

### FBR3 — deployment identity migration

- [ ] `temote-mcp-gateway` -> `temote-fabric`
- [ ] health/server identity migration
- [ ] old/new endpoint coexistence strategy
- [ ] Access / secrets / route migration
- [ ] DO migration verified
- [ ] rollback verified

### FBR4 — source tree rename

Only after FBR1-FBR3 are safe:

- [ ] `gateway/` -> `fabric/`
- [ ] contract/test/deploy paths updated
- [ ] docs updated
- [ ] release/preflight scripts updated
- [ ] stale `gateway` naming removed except compatibility/history

## 17. Acceptance

The naming/migration is complete when:

- [ ] a new user can understand Temote Host vs Temote Fabric without learning the old Gateway architecture
- [ ] Fabric is not described as Task/Execution authority
- [ ] local execution works with Fabric unavailable
- [ ] a Host can reconnect and catch observation sync up idempotently
- [ ] remote callers can route to Hosts through Fabric
- [ ] repository context can be read through Fabric up to the last synced revision while Hosts are offline
- [ ] current gateway users have a non-destructive migration path
- [ ] no DO/D1 state is silently lost because of the rename
- [ ] public contract changes are intentional and versioned
- [ ] code separates routing, observation, context, and memory responsibilities

## 18. Principle

> **Temote brings remote work to hand. Temote Fabric keeps those hands, hosts, agents, and contexts connected.**

In implementation terms:

> Execute on the Host. Connect through the Fabric. Preserve context across both.
