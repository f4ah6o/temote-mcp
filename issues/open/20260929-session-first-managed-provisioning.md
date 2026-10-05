# S1: session-first managed repository provisioning

Status: open
Model: unknown
Created: 2026-09-29
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Start managed sessions from typed repository identity with idempotent provisioning, retaining explicit existing-workspace compatibility.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

Path-first creation exposes host layout and cannot safely reconcile lost provisioning responses.

## 目標

Start managed sessions from typed repository identity with idempotent provisioning, retaining explicit existing-workspace compatibility.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

For `session_start`, the repository-managed `source` form and existing named-root `path` form are mutually exclusive; reject requests containing both or neither. The managed form requires a caller-supplied `operation_id` before any session-owned side effect and persists an Accepted receipt before provisioning. Retain the path form as explicit `ExistingWorkspace` compatibility. S0a's initial implementation merged in PR #86; the grammar and checked-entry-point conformance packet remains unfinished.

### Preserved fixed contract: 2. Decision

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

## 受け入れ条件

Complete source criteria from “15. Acceptance for the architecture” (unchecked items remain unverified):

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
- [ ] managed `session_start` requires a caller-supplied `operation_id` and never double-creates Session / Workspace across lost responses, retries, crashes, or concurrent resends (§5.3).

### Provisioning retry scenarios (受入 test 仕様)

以下は S1〜S3a 実装 packet が automated test として実装する受入仕様であり、実行済み test の報告ではない。

- 初回成功後に応答だけ喪失 → 同じ key の再送は同じ SessionId / WorkspaceId / result を返す
- 同じ key の同時到着 → provisioning は二重化しない
- 同じ key + 異なる request → `operation_conflict`、追加副作用なし
- Accepted receipt 確立後の crash → 同じ operation として再開・照合
- workspace 作成後・Completed receipt 保存前の crash → 根拠なく再作成せず、安全に照合または `reconciliation_required` で fail closed
- remote base 更新後の retry → 初回に pin した revision を維持する
- 異なる authenticated caller から同じ key → 他者の receipt / Session を取得できない
- 明示的に新しい key → policy の範囲内で別 Session を作成可能

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd gateway && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

## リスク

- Preserve dirty/ahead/diverged checkouts and operation receipts; ambiguous side effects require reconciliation rather than a blind retry.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.
- 2026-10-05: The selected contract is `source` XOR `path`, with `operation_id` required for managed repository creation. Do not describe the S0a initial merge as its strengthened conformance being complete.

## 2026-10-05 実行パケット

- [`repository-store-idempotent-ensure`](../polished/20261005-repository-store-idempotent-ensure.md)
- [`managed-workspace-allocation`](../polished/20261005-managed-workspace-allocation.md)
- [`managed-session-source-start`](../polished/20261005-managed-session-source-start.md)

These are planned packets, not completed implementation. The parent remains open until applicable children and acceptance evidence are complete.

## 既存設計・履歴

> Historical Status: design ready / S0a initial implementation merged (PR #86); strengthened-contract conformance pending (`issues/polished/20260929-s0a-contract-conformance.md`)

Repository: `f4ah6o/temote-mcp`  
> Historical Created: 2026-09-29 (Asia/Tokyo)

Revision note (2026-09-29, PR #85 review follow-up): added the §5.3 provisioning retry contract — required caller-supplied `operation_id`, durable Accepted receipt before the first session-owned side effect, replay / `operation_conflict` / `reconciliation_required` semantics. The inherited F1 `RepositoryId` component grammar lives on the S0a packet side.

Related:
- `issues/open/20260924-temote-development-harness-restructure.md`
- `issues/open/20260925-vcs-transaction-jj-first.md`
- `issues/done/20260925-v2-vcs-workspace-contract.md`
- `issues/open/20260926-named-root-workspace-identity.md`
- `issues/closed/20260927-instruction-side-bare-repo-provisioning.md`
- `issues/open/20260926-task-change-orchestration-stacked-pr.md`
- `issues/polished/20260929-s0a-typed-session-source-contract.md` (first behavior-preserving implementation packet)

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
!= OperationId
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
  operation_id = "<caller-generated-uuid>",   # required retry key (§5.3)
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
temote session start f4ah6o/temote-mcp --operation-id <uuid>
temote session start f4ah6o/temote-mcp --operation-id <uuid> --base main
temote session start f4ah6o/temote-mcp --operation-id <uuid> --vcs jujutsu
```

これらの MCP/CLI 形式は将来 contract の例であり、現行の公開 `session_start(path=...)` 機能ではない。

`operation_id` は caller-supplied の必須 retry key (§5.3)。timeout / 応答喪失 / transport retry では同じ値を再送し、意図的に別 Session を作る場合だけ新しい値を使う。

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

### 5.3 Provisioning operation の retry contract

managed `session_start` は repository preparation と workspace allocation を伴う副作用操作である。
応答喪失や transport retry の時点で caller は server 発行の SessionId をまだ受け取っていないため、server 発行 ID だけでは「同じ要求の再送」と「同じ repository の新規 Session」を区別できない。
そこで caller-supplied `operation_id` を必須 retry key とする (task_start の `operation_id` 必須規約、F1 の request fingerprint、V2 の receipt / `reconciliation_required` と同じ契約系)。

#### operation_id の発行と scope

- caller は初回送信前に `operation_id` を生成する (UUID を推奨)。
- timeout / 応答喪失 / transport retry では同じ値を再送する。
- 意図的に別 Session を作る場合だけ新しい値を使う。
- provisioning 前は Session が存在しないため、receipt lookup は `operation_id` を一次参照 key とし、既存 SessionId を必須前提にしない。
- key は (authenticated caller namespace, target host, operation kind) に bind する。`operation_id` 単体を権限や identity として扱わず、request 認可は従来どおり caller の permission で行う。
- local CLI と remote/MCP で同じ契約とし、transport の違いだけで同じ operation が別扱いにならないようにする。

#### request fingerprint

receipt は normalized request fingerprint を保持する。fingerprint の対象:

- operation kind (managed repository session start)
- normalized `RepositoryId` (host / owner / name)
- requested base (caller が指定した ref 名。省略時は省略の事実)
- VCS preference
- managed start のその他の caller 入力 (workspace policy 等の選択)

fingerprint に含めないもの:

- remote HEAD / remote-tracking ref の観測値
- 解決済み・pinned commit (pinned execution state であり request ではない)
- server 発行の SessionId / WorkspaceId
- timestamp など再送で変動する値

同じ (caller namespace, host, operation kind, `operation_id`) に対して:

- 同じ fingerprint の再送 → 同一 operation として replay / reconcile
- 異なる fingerprint (`repository` / `base` / `vcs` 等が違う) → `operation_conflict` で fail closed。追加副作用を起こさない

初回に pin した base revision は receipt に保持する。remote が進んでも、同じ operation の再送で base を再選択しない。

#### durable acceptance ordering

- session-owned provisioning の最初の副作用 (SessionId 採番、WorkspaceId 採番、workspace 作成) より先に、operation ownership と Accepted receipt を durable に確立する。
- SessionId / WorkspaceId の採番は receipt に結び付け、同時再送でも別の割当を作らない。
- Accepted receipt が存在する途中状態は「同じ operation の再開・照合対象」とし、新規 provisioning の根拠にしない。
- RepositoryStore ensure/fetch は共有 substrate であり F1 の idempotent ensure/fetch 契約に従う。session としての provisioning 副作用はすべて receipt 確立の後に置く。

#### 応答喪失 / crash 後の規則

- Completed receipt を持つ再送は、記録済みの identity (SessionId / WorkspaceId / pinned base / result) をそのまま返す。
- Accepted receipt のみ存在する再送は、安全に照合できる範囲で同じ operation として再開する。照合できない場合は `reconciliation_required` を返して fail closed し、根拠なく新規 provisioning を開始しない。
- 対象 Session が既に停止・終了していても、同じ key の再送で別 Session を暗黙作成しない。
- receipt は対応 Session record の retained lifetime 以上保持する。期限切れ / prune 済みの key の再送は無条件に新規要求として受理せず、明示的な conflict / expired 応答で fail closed する。

#### caller surface と責務配置

- CLI / MCP / local / remote は同一 contract とし、CLI の retry は同じ `--operation-id` を明示して行う。
- receipt の確立は最初の managed provisioning 副作用より前の責務であり、S3a (caller surface) まで durability を後回しにしない。
- S0a の `SessionStartSpec` は "source の記述" (`RepositoryId` / `base` / `vcs`) であり、`operation_id` は別の request envelope に置く。source identity と invocation identity を分離する。
- この責務境界は文書化のみとし、本 issue では runtime を実装しない。

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
provisioning ownership は caller-supplied `operation_id` の Accepted receipt で、最初の session-owned 副作用より前に確立する (§5.3)。receipt の無い workspace allocation を発生させない。

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
10. provisioning retry は caller-supplied `operation_id` + durable operation receipt で idempotent にする (§5.3)。receipt は最初の session-owned 副作用より前に確立し、応答喪失・crash・同時再送で Session / Workspace を二重作成しない。同じ key + 異なる request は `operation_conflict`、照合不能は `reconciliation_required` で fail closed。

## 12. Compatibility migration

### Phase S0 — typed internal split

behavior-preserving:

- add `SessionStartSpec::ManagedRepository` / `ExistingWorkspace`
- add repository source normalization
- no existing public path start removal
- no workspace auto-provisioning yet

### Phase S1 — provisioning planner

- session request (caller-supplied `operation_id`) owns the provisioning operation
- establish Accepted receipt / operation ownership before the first session-owned side effect (§5.3)
- resolve configured repository-store root / workspace pool
- ensure store
- fetch / pin base (pinned revision は receipt に記録し、同じ operation の再送で再選択しない)
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

S0a landed via PR #86; conformance to the strengthened §3 contract is tracked by `issues/polished/20260929-s0a-contract-conformance.md` and must land before S1+ packets wire `SessionStartSpec` / `RepositoryId` into managed `session_start`.

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

- public MCP/CLI composition (required caller-supplied `operation_id`, §5.3)
- expose replay / `operation_conflict` / `reconciliation_required` retry semantics on the caller surface (receipt durability は S1 で確立済み。S3a へ先送りしない)
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
- [ ] managed `session_start` requires a caller-supplied `operation_id` and never double-creates Session / Workspace across lost responses, retries, crashes, or concurrent resends (§5.3).

### Provisioning retry scenarios (受入 test 仕様)

以下は S1〜S3a 実装 packet が automated test として実装する受入仕様であり、実行済み test の報告ではない。

- 初回成功後に応答だけ喪失 → 同じ key の再送は同じ SessionId / WorkspaceId / result を返す
- 同じ key の同時到着 → provisioning は二重化しない
- 同じ key + 異なる request → `operation_conflict`、追加副作用なし
- Accepted receipt 確立後の crash → 同じ operation として再開・照合
- workspace 作成後・Completed receipt 保存前の crash → 根拠なく再作成せず、安全に照合または `reconciliation_required` で fail closed
- remote base 更新後の retry → 初回に pin した revision を維持する
- 異なる authenticated caller から同じ key → 他者の receipt / Session を取得できない
- 明示的に新しい key → policy の範囲内で別 Session を作成可能

## 16. Principle

> Start a development session from intent and repository identity. Let Temote allocate the working substrate. Treat path, branch, and executor as projections/resources of that session, not as the session itself.
