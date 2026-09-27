# Add instruction-side bare repository provisioning flow

Status: open  
Repository: `f4ah6o/temote-mcp`  
Created: 2026-09-27 (Asia/Tokyo)

Related:
- `issues/open/20260925-f1-repository-store-workspace-contract.md`
- `issues/open/20260925-v2-vcs-workspace-contract.md`
- `issues/open/20260926-named-root-workspace-identity.md`

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

A caller should be able to start from repository identity rather than from an already-prepared local checkout.

Conceptually:

```text
instruction
  repo = github.com/f4ah6o/example
  task = implement X

Temote
  -> resolve allowed repository-store root
  -> ensure bare repository store
  -> fetch according to freshness policy
  -> ensure task workspace
  -> acquire reservation / bind task
  -> start session / delegated agent
```

The caller should not need to know the host's absolute store path or manually run `git clone --bare`, `jj git clone`, or a `gh-git` store command beforehand.

## 4. API / UX direction

The exact surface can be decided during implementation, but it should support both an explicit provisioning operation and composition with task/session creation.

Possible explicit form:

```text
repository_prepare(
  repository = "github.com/f4ah6o/example"
)
```

Possible composed form:

```text
session_start(
  repository = "github.com/f4ah6o/example",
  workspace = {
    id = "task-123",
    branch = "feat/task-123"
  }
)
```

where Temote internally ensures the repository store before creating the managed workspace.

CLI equivalents may be provided, but the remote/MCP instruction path is the primary requirement.

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

### P1 — instruction-side surface

- [ ] expose repository preparation through the supported MCP/remote instruction path
- [ ] make the operation idempotent
- [ ] provide actionable errors for missing root/config/auth/repository conflicts
- [ ] ensure callers do not need host-local absolute paths

### P2 — compose with managed workspace/session creation

- [ ] allow task/session creation from repository identity when the bare store does not yet exist
- [ ] ensure/fetch store according to freshness policy
- [ ] create or reuse the managed task workspace
- [ ] bind repository/workspace/session/task identities
- [ ] acquire reservations before mutating shared repository/workspace state

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
- [ ] Both explicit repository preparation and composed task/session startup have a documented contract.
- [ ] Tests cover first provisioning, reuse, conflict, and legacy-checkout preservation.

## 10. Non-goals

- Reimplementing bare Git/JJ storage directly inside the instruction protocol.
- Allowing arbitrary filesystem paths from remote callers.
- Automatically migrating existing primary checkouts.
- Solving repository-store garbage collection or retention in this issue.

## 11. Principle

> The instruction side should name the repository it wants to work on; Temote should own preparing the managed repository substrate needed to do that work safely.
