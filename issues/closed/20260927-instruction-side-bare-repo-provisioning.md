# Add instruction-side bare repository provisioning flow

Status: closed — caller-flow architecture superseded by `issues/open/20260929-session-first-managed-provisioning.md`; this document is retained as historical RepositoryStore provisioning design
Repository: `f4ah6o/temote-mcp`  
Created: 2026-09-27 (Asia/Tokyo)

Related:
- `issues/done/20260925-f1-repository-store-workspace-contract.md`
- `issues/done/20260925-v2-vcs-workspace-contract.md`
- `issues/open/20260926-named-root-workspace-identity.md`

## 0. 2026-09-29 architecture correction

The repository preparation requirements below remain valid, but the top-level ordering changes.

Old framing:

```text
repository identity
  -> bare repository store
  -> managed workspace
  -> session / task
```

Session-first framing:

```text
session request
  -> repository source normalization
  -> bare RepositoryStore ensure/fetch
  -> base pin
  -> managed VCS workspace allocation
  -> agent execution
```

For the Jujutsu normal path, managed workspace allocation means **jj workspace directly from the Git-backed store**. It does not first create a Git branch/worktree. Git branch/worktree remains the explicit Git compatibility backend. See `issues/open/20260929-session-first-managed-provisioning.md`.

## 1. Problem

The repository-store design already defines a bare repository store primitive, but Temote currently has no clear **instruction-side flow for provisioning that bare repository before work starts**.

In other words, the substrate may support:

```text
repository identity
    ->
bare repository store
    ->
managed workspace
    ->
session / task
```

but the caller issuing the instruction does not have one obvious Temote-level operation that says:

> prepare the repository store for this repository, then create/use a workspace and start the task.

Today this leaves an operational gap: the user or another local tool must arrange the bare repository/store out of band before the normal Temote flow can use it.

## 2. Why this is a Temote issue

`gh-git` should own the low-level repository/store primitives.

Temote should own:

- repository selection and identity
- named-root / filesystem admission
- reservation and task ownership
- deciding when repository provisioning is needed
- invoking the repository-store primitive
- binding the resulting repository/workspace to the session/task
- exposing the capability to the instruction side through a stable API/tool/CLI flow

The missing piece is therefore not another bare-Git implementation. It is the Temote orchestration and caller-facing entry point.

## 3. Desired caller flow

This retained child issue now covers the RepositoryStore part of a session-first request.

Conceptually:

```text
session request
  source = github.com/f4ah6o/example
  task = implement X

Temote
  -> create/accept session provisioning operation
  -> resolve allowed repository-store root
  -> ensure bare repository store
  -> fetch according to freshness policy
  -> pin base revision
  -> hand the result to the managed workspace allocator
  -> start runtime / delegated agent only after workspace readiness
```

The caller should not need to know the host's absolute store path or manually run `git clone --bare`, `jj git clone`, or a `gh-git` store command beforehand.

## 4. API / UX direction

The low-level adapter may expose an internal typed repository preparation operation:

```text
ensure_repository(
  repository = "github.com/f4ah6o/example"
)
```

The normal caller surface is the session-first composition defined by `20260929-session-first-managed-provisioning.md`:

```text
session_start(
  operation_id = "<caller-generated-uuid>",   # required retry key (parent §5.3)
  source = {
    kind = "repository",
    repository = "github.com/f4ah6o/example",
    base = "main"
  },
  vcs = "auto"
)
```

The composed operation follows the parent document's retry/idempotency contract (§5.3): the caller generates `operation_id` before the first send and reuses the same value on timeout / lost response / transport retry; a durable Accepted receipt is established before the first session-owned provisioning side effect; same key + same normalized request fingerprint replays or reconciles; same key + a different request fails closed as `operation_conflict`. The internal `ensure_repository` step participates in that operation's receipt rather than defining a separate idempotency scope.

No branch or physical workspace path is required from the caller for the Jujutsu normal path. The store adapter returns repository/freshness/base evidence to the session provisioning flow; workspace allocation is a later step owned by Temote.

CLI equivalents may be provided, but local and remote paths must use the same orchestration contract.

## 5. Repository identity vs filesystem identity

Do not make the caller provide an arbitrary absolute bare-repository path.

The instruction side should provide logical repository identity, for example:

```text
github.com/f4ah6o/temote-mcp
```

Temote then resolves the host-local store location under an administrator-approved root/store namespace.

This should compose with the named-root contract:

- named root = filesystem admission boundary
- repository identity = logical source repository
- bare store path = host-local implementation detail
- workspace identity = concrete task workspace

These identities must not be collapsed into one path string.

## 6. Safety / invariants

- Do not convert an existing normal checkout into a bare repository implicitly.
- Do not reset, clean, stash, move, or delete an existing user checkout.
- Do not accept arbitrary caller-controlled absolute store paths.
- Do not let a remote caller broaden the configured filesystem/root authority.
- Reuse an existing compatible bare store idempotently.
- Fail explicitly on repository/store identity conflict.
- Preserve reservation/admission rules before mutating repository/workspace state.
- Keep VCS-specific mechanics behind the repository-store adapter so the Temote flow is not hard-coded to raw Git commands.

## 7. Relationship to existing repository-store contract

`20260925-f1-repository-store-workspace-contract.md` already specifies the low-level `store ensure`, `store fetch`, and workspace primitives and assigns repository/workspace selection plus reservation ownership to Temote.

This issue fills the missing caller-to-orchestration path:

```text
instruction side
    |
    v
Temote repository provisioning orchestration
    |
    v
repository-store adapter / gh-git primitive
```

It should not duplicate or replace the existing F1/V2 contracts.

## 8. Implementation packets

### P0 — expose repository preparation internally

- [ ] add one Temote internal operation for `ensure repository store`
- [ ] take logical repository identity, not a raw filesystem path
- [ ] resolve the configured store root on the host
- [ ] call the repository-store adapter
- [ ] return structured repository/store identity and evidence

### P1 — session provisioning integration

- [ ] call repository preparation from the session-first provisioning operation
- [ ] make the operation idempotent under the parent contract: caller-supplied `operation_id`, durable Accepted receipt before the first session-owned side effect, fingerprint replay / `operation_conflict` / `reconciliation_required` semantics
- [ ] provide actionable errors for missing root/config/auth/repository conflicts
- [ ] ensure callers do not need host-local absolute paths

### P2 — hand off to managed workspace provisioning

- [ ] allow Session provisioning from repository identity when the bare store does not yet exist
- [ ] ensure/fetch store according to freshness policy and pin the base revision
- [ ] pass the normalized RepositoryId/store/base result to the workspace allocator
- [ ] for Jujutsu, do not create a Git branch/worktree in this packet
- [ ] bind repository/store observation to the owning session provisioning operation

### P3 — parity and tests

- [ ] local CLI and remote/MCP paths use the same orchestration
- [ ] first-use repository provisioning E2E
- [ ] existing-store reuse E2E
- [ ] conflict/failure cases
- [ ] no existing checkout mutation regression test
- [ ] named-root/store-root containment tests

## 9. Acceptance criteria

- [ ] From the instruction side, a caller can name a repository that has no prepared local bare store and start the Temote-managed flow without manual host-side repository setup.
- [ ] Temote provisions or reuses the bare repository store through the repository-store adapter.
- [ ] The caller does not provide or need to know the host-local bare-store path.
- [ ] The resulting repository/store identity is available to subsequent workspace/session/task operations.
- [ ] A compatible existing store is reused idempotently.
- [ ] Conflicting or unsafe existing state fails closed without modifying the user's normal checkout.
- [ ] The flow obeys named-root/store-root admission and Temote reservation rules.
- [ ] Internal repository preparation and composed session-first startup have a documented contract.
- [ ] Tests cover first provisioning, reuse, conflict, and legacy-checkout preservation.

## 10. Non-goals

- Reimplementing bare Git/JJ storage directly inside the instruction protocol.
- Allowing arbitrary filesystem paths from remote callers.
- Automatically migrating existing primary checkouts.
- Solving repository-store garbage collection or retention in this issue.

## 11. Principle

> The instruction side should name the repository it wants to work on; Temote should own preparing the managed repository substrate needed to do that work safely.
