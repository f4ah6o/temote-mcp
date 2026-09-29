# S1: session-first managed repository provisioning

Status: design ready / implementation packets defined  
Repository: `f4ah6o/temote-mcp`  
Created: 2026-09-29 (Asia/Tokyo)

Related:
- `issues/open/20260924-temote-development-harness-restructure.md`
- `issues/open/20260925-vcs-transaction-jj-first.md`
- `issues/done/20260925-v2-vcs-workspace-contract.md`
- `issues/open/20260926-named-root-workspace-identity.md`
- `issues/open/20260927-instruction-side-bare-repo-provisioning.md`
- `issues/open/20260926-task-change-orchestration-stacked-pr.md`

## 1. Problem

Temote の session API は歴史的に既存 directory / checkout を起点にしている。

```text
path
  -> session
      -> task
          -> agent
```

このモデルは local-machine MCP としては自然だったが、development orchestrator としては次の無理が出る。

- caller が host-local path / checkout layout を知る必要がある
- repository identity と workspace identity と cwd が混ざる
- local main / dirty checkout / stale checkout を session 起動前に人間が管理する必要がある
- bare repository store を導入しても、caller flow が checkout-first のままになる
- jj-first にしても先に Git branch / git worktree を作ると Git と jj の二重 workspace model になる
- Devin Cloud のような hosted execution を「directory を持つ session」という形へ無理に寄せる

Temote の一次 object を directory から session へ反転する。

## 2. Decision

### 2.1 Session is primary

新規 managed flow の product-level model は以下とする。

```text
Session
  |
  +-- RepositorySource
  +-- WorkspaceBinding
  |     |
  |     +-- host-managed workspace
  |     |      +-- WorkspaceVcs
  |     |
  |     +-- hosted workspace
  |
  +-- Task(s)
  +-- Execution(s)
  +-- Observation / Context
  +-- Verification
  +-- Delivery
```

repository / physical directory は Session が確保する resource であり、Session identity ではない。

### 2.2 Normal host-local path

新規 Temote-managed repository の normal path:

```text
session request
  -> normalize repository identity
  -> ensure / fetch bare Git RepositoryStore
  -> pin base revision
  -> allocate Temote workspace id
  -> create jj workspace
  -> create / bind logical jj change
  -> start agent execution
```

**Jujutsu backend では session start 時に Git branch を作らない。**
**Jujutsu backend では session start 時に Git worktree を作らない。**

Git bookmark / branch は delivery boundary で materialize する。

```text
jj logical change
  -> verified materialized revision
  -> delivery bookmark / Git ref
  -> push
  -> PR / stack
```

### 2.3 Git is compatibility backend

Git backend を削除しない。

```text
VcsBackend::Jujutsu
  -> bare Git store + jj workspace        # managed default

VcsBackend::Git
  -> bare Git store + Git branch/worktree # explicit compatibility
```

Git backend への fallback は capability / caller policy による明示的な state とし、silent fallback は禁止する。

## 3. Identity model

以下を同一視しない。

```text
SessionId
!= RepositoryId
!= WorkspaceId
!= TaskId
!= ChangeId
!= ExecutionId
!= jj workspace name
!= jj change_id
!= jj commit_id
!= Git branch/bookmark
!= GitHub PR
!= physical path
```

### 3.1 Repository identity

既存 F1/V2 と同じく:

```text
RepositoryId = host / owner / name
```

例:

```text
github.com/f4ah6o/temote-mcp
```

### 3.2 Workspace identity

`WorkspaceId` は Temote が発行・所有する durable id。

physical workspace path は host-local projection:

```text
WorkspaceId
   |
   +-- host A -> /Volumes/.../temote/workspaces/<id>
   +-- host B -> /home/.../temote/workspaces/<id>
```

caller-facing durable identity に absolute path を使わない。

### 3.3 Session identity

Session は repository より広い orchestration boundary。

1 session が通常 1 primary repository workspace を持つことはできるが、その制約を Session identity 自体には埋め込まない。

将来の multi-repository task / hosted execution を阻害しない。

## 4. Named roots: identity ではなく admission

named root は残す。ただし役割を変更する。

従来の方向:

```text
named-root logical path
  -> workspace identity
  -> session
```

target:

```text
Session
  -> Workspace allocator
       -> configured named-root-backed workspace pool
            -> generated physical workspace path
```

named root が答えるもの:

> この Host で Temote が workspace/store を配置してよい filesystem authority はどこか。

named root が答えないもの:

> この Session / repository / workspace の durable identity は何か。

したがって host-local managed workspace は引き続き configured root containment を満たすが、caller が `src/foo` のような path を指定することを normal managed flow の前提にしない。

既存 checkout / compatibility session では named-root-relative path を引き続き logical location coordinate として利用する。

## 5. Caller contract

### 5.1 New managed form

概念 API:

```text
session_start(
  source = {
    kind = "repository",
    repository = "github.com/f4ah6o/temote-mcp",
    base = "main"
  },
  vcs = "auto"   # resolves to jujutsu or explicit unsupported result
)
```

CLI direction:

```sh
temote session start f4ah6o/temote-mcp
temote session start f4ah6o/temote-mcp --base main
temote session start f4ah6o/temote-mcp --vcs jujutsu
```

caller は bare store path / workspace path / Git branch を指定しない。

### 5.2 Existing-workspace compatibility form

既存 path 起点を即座に削除しない。

内部 contract を明確に分ける。

```text
SessionStartSpec
  +-- ManagedRepository { source, base, vcs_policy, ... }
  +-- ExistingWorkspace { named_root_relative_path, ... }
```

migration 中の public `session_start` は mutually-exclusive input として両方を受けてもよい。

最終的には path-based start を `--existing-workspace` / attach 系の explicit compatibility semantics に寄せ、managed default と混同しない。

absolute path を remote caller の authority input として追加しない。

## 6. Provisioning state

Session を一次 object にするため、workspace provisioning は session の内部 phase として観測できる必要がある。

conceptual state:

```text
Session
  provisioning:
    requested
    -> repository_preparing
    -> base_resolved
    -> workspace_allocating
    -> workspace_ready
    -> failed

  runtime:
    not_started
    -> starting
    -> active
    -> stopping
    -> stopped / crashed
```

既存 lifecycle enum を直ちに壊す必要はない。
最初の implementation slice では provisioning receipt / record を別 record として持ち、runtime は workspace_ready 後だけ生成してよい。

重要なのは、workspace が先に存在して偶然 session が紐づくのではなく、**session request / operation が provisioning ownership を持つ**こと。

## 7. RepositoryStore contract

bare Git store は repository identity ごとに共有可能。

```text
RepositoryStore
  repository_id
  origin
  remote observations
  fetch state
  object database
```

store 自体は Session-owned ではない。

Session-owned なのは:

- base observation / pinned revision
- workspace allocation
- reservation
- logical change
- execution correlation

複数 Session が同じ bare store を安全に共有できる。

## 8. Jujutsu workspace contract

Jujutsu managed default では:

```text
bare Git store
   |
   +-- jj workspace: session/workspace A
   |      -> logical change A
   |
   +-- jj workspace: session/workspace B
          -> logical change B
```

Temote が行う:

- workspace name / path allocation
- base revision pin
- logical change creation / discovery
- snapshot points
- operation-id correlation
- writer reservation
- verification binding
- delivery materialization

agent に normal path で要求しない:

- `git add`
- `git commit`
- `git switch`
- `git checkout`
- `git worktree`
- `git rebase`
- `jj new`
- `jj commit`
- delivery bookmark creation
- push

agent は files / tests / build を扱う。

## 9. Agent / task relationship

workspace / change は executor より長命。

```text
Session S
  Workspace W
    Change C

Task T
  Execution 1 -> Codex
  Execution 2 -> OpenCode
  Execution 3 -> Devin
```

executor replacement で workspace/change identity を変えない。

independent review unit を作る場合のみ別 Change / Workspace を allocate し、既存 Task/Change graph issue の dependency model に従う。

## 10. Hosted execution

Devin Cloud 等では local workspace allocation を必須にしない。

```text
WorkspaceBinding
  +-- HostManaged {
  |      repository_id,
  |      workspace_id,
  |      vcs_state,
  |      physical_path_projection
  |   }
  |
  +-- Hosted {
         provider,
         repository_id,
         provider_workspace_ref
      }
```

これにより Session contract は caller location / executor location から独立する。

## 11. Safety / invariants

1. Existing checkout を managed bare store / jj workspace へ暗黙変換しない。
2. Existing dirty checkout を reset / clean / stash / move / delete しない。
3. Remote caller に arbitrary absolute workspace/store path を指定させない。
4. RepositoryStore は idempotent ensure + explicit freshness observation。
5. Session start は base revision を pin してから writable workspace を agent に渡す。
6. 1 independently writable Change -> 1 writable managed workspace。
7. jj managed flow で mutating Git と mutating jj を unrestricted に混在させない。
8. Git branch/bookmark は Session / Workspace identity にしない。
9. agent completion != verification success != delivery success。
10. provisioning retry は operation receipt で idempotent にする。

## 12. Compatibility migration

### Phase S0 — typed internal split

behavior-preserving:

- add `SessionStartSpec::ManagedRepository` / `ExistingWorkspace`
- add repository source normalization
- no existing public path start removal
- no workspace auto-provisioning yet

### Phase S1 — provisioning planner

- session request owns provisioning operation id
- resolve configured repository-store root / workspace pool
- ensure store
- fetch / pin base
- produce a machine-readable plan
- no agent start until plan is complete

### Phase S2 — jj workspace provisioning

- use VCS core to create/reuse jj workspace
- create / bind logical change
- persist Session <-> Repository <-> Workspace <-> Change mapping
- start runtime in generated workspace
- physical path appears only in local diagnostics / backend launch

### Phase S3 — caller surface

- repository-first MCP / CLI start
- `session_info` exposes logical source + workspace ids + base revision + backend
- path-based input is explicitly ExistingWorkspace compatibility
- local and remote caller paths share orchestration

### Phase S4 — delivery boundary

- verified jj revision -> bookmark/ref
- push / PR / stack through delivery adapter
- no branch required before delivery

### Phase S5 — migration / deprecation

- normal documentation stops teaching checkout-first managed sessions
- existing-workspace path remains explicit compatibility surface until removal is separately approved
- legacy checkout preservation tests remain mandatory

## 13. First implementation packet

The first code packet should be intentionally small and behavior-preserving.

### S0a — typed session source contract

Scope:

- define typed repository source / session-start spec in orchestration/core-facing code
- normalize `owner/repo`, `host/owner/repo`, HTTPS / SSH repository source into RepositoryId
- distinguish ManagedRepository vs ExistingWorkspace
- add serialization / validation tests
- do not change current MCP wire contract yet
- do not provision a repository
- do not move or mutate a checkout

Acceptance:

- [ ] no core API needs to infer repository identity from cwd for ManagedRepository
- [ ] no raw absolute path exists in ManagedRepository input
- [ ] ExistingWorkspace preserves current named-root semantics
- [ ] invalid/ambiguous repository source fails closed
- [ ] types leave room for hosted workspace binding
- [ ] existing session lifecycle tests remain green

This packet creates the seam required before repository provisioning is wired into session start.

## 14. Follow-on implementation packets

### S1a — RepositoryStore adapter

- expose typed `ensure_repository(source)`
- host-configured store root
- idempotent ensure/fetch
- pinned base observation
- no user checkout mutation

### S2a — Workspace allocator

- configured named-root-backed workspace pool
- generated WorkspaceId and path
- containment / reservation
- no caller-selected arbitrary path

### S2b — jj workspace create/reuse

- bare store -> jj workspace
- logical change bind
- snapshot/reconcile integration
- Git branch/worktree absent from Jujutsu normal path

### S3a — repository-first session start

- public MCP/CLI composition
- provisioning receipt / retry
- session_info projection
- compatibility path explicitly marked ExistingWorkspace

### S4a — delivery ref materialization

- verification-bound revision -> bookmark / Git ref
- push/PR
- retry/reconcile

## 15. Acceptance for the architecture

- [ ] A new repository with no normal local checkout can be named by repository identity and started as a managed Session.
- [ ] Temote creates/reuses a bare RepositoryStore without creating local `main`.
- [ ] Jujutsu managed default creates a jj workspace directly; no Git branch/worktree is created at session start.
- [ ] Agent edits are captured through Temote/jj snapshot boundaries without requiring agent-authored commits.
- [ ] Session/Workspace/Change survive executor replacement.
- [ ] Delivery creates/materializes Git ref only after verification.
- [ ] Git backend still supports explicit compatibility worktree semantics.
- [ ] caller does not need host-local physical paths for managed repository start.
- [ ] named roots remain enforced as host filesystem admission, not durable workspace identity.
- [ ] Existing dirty/unmanaged checkout is never silently migrated or modified.

## 16. Principle

> Start a development session from intent and repository identity. Let Temote allocate the working substrate. Treat path, branch, and executor as projections/resources of that session, not as the session itself.
