# Require named-root-backed workspace identity for all managed sessions

Status: design ready / implementation not started  
Repository: `f4ah6o/temote-mcp`  
Related:
- `issues/open/20260924-temote-development-harness-restructure.md`
- `issues/open/20260925-f1-repository-store-workspace-contract.md`
- `issues/open/20260925-v2-vcs-workspace-contract.md`
- `issues/open/20260926-temote-fabric-product-boundary.md`
Created: 2026-09-26 (Asia/Tokyo)

## 1. Decision

Make **named-root-backed workspace identity mandatory for every managed Temote session**.

However, do **not** make the environment variable `TEMOTE_MCP_ROOTS` itself the architectural requirement.

The invariant should be:

> Every managed session workspace resolves through a configured named-root registry to `<root-name>/<relative-path>`.

`TEMOTE_MCP_ROOTS` remains one supported source for populating that registry.

Future sources may include a Temote config file or explicit root-management CLI.

## 2. Current inconsistency

Managed/remote sessions already use named-root-relative paths:

```text
TEMOTE_MCP_ROOTS='src=~/src'

session_start(
  path="src/temote-mcp",
  session_id="temote"
)
```

Current documentation also states that missing named-root configuration fails closed and that HOME, `/`, cwd, and repository fallback are not used.

At the same time, the compatibility/local CLI still permits:

```sh
cd ~/src/temote-mcp
temote-mcp start temote
```

where cwd is passed to the supervisor as the workspace.

That leaves two workspace identity models:

1. logical named-root-relative identity
2. ambient cwd / absolute-path identity

The second model should be removed as an authority model.

## 3. Why named roots should be mandatory

### 3.1 One workspace identity model

Temote should identify a workspace logically:

```text
src/temote-mcp
```

rather than by a host-specific physical path:

```text
/Volumes/devstorage/Developer/temote-mcp
/home/fu2hito/src/temote-mcp
```

Physical paths remain necessary locally, but they should be implementation details resolved from the root registry.

### 3.2 Local and remote become equivalent

The caller should not change the workspace contract.

These should resolve through the same code path:

```text
local CLI
local MCP
remote MCP
Fabric-routed caller
Codex/OpenCode/Devin backend
```

The difference is transport and authority, not workspace identity.

### 3.3 Fail closed

Using cwd directly makes ambient process state part of authority.

A shell opened in the wrong directory can accidentally create a valid Temote session.

A required named root gives an explicit administrative boundary before Temote can manage or delegate work there.

### 3.4 Multi-host portability

The same logical root can map to different physical paths per Host:

```text
mac-main:
  src -> /Volumes/devstorage/Developer

linux-main:
  src -> /home/fu2hito/src
```

Both can still refer to:

```text
src/temote-mcp
```

without requiring the physical path to match.

### 3.5 Durable metadata

Observation, context, memory, VCS state, checkpoints, delegation continuation, and Fabric replication should not primarily key durable state by machine-specific absolute cwd.

A named-root-relative identity is a better stable coordinate.

## 4. Important distinction: root registry vs environment variable

Do not encode this requirement as:

> `TEMOTE_MCP_ROOTS` must always be present in the process environment.

Encode it as:

> Temote must have a valid named-root registry before starting a managed session.

Today that registry may come from:

```text
TEMOTE_MCP_ROOTS
```

Future configuration should be able to come from something like:

```text
~/.config/temote/config.toml

temote root add src ~/src
temote root list
temote root remove src
```

without changing session semantics.

This avoids making shell/service environment injection a permanent product-level requirement.

## 5. Canonical workspace identity

A managed workspace should have a canonical logical identity:

```text
root_name     = "src"
relative_path = "temote-mcp"
logical_path  = "src/temote-mcp"
```

and may additionally carry host-local resolved data:

```text
canonical_physical_path =
  "/Volumes/devstorage/Developer/temote-mcp"
```

The logical identity is durable/product-facing.

The canonical physical path is host-local execution data and diagnostics.

Do not use the absolute path as the cross-host identity.

## 6. Proposed session-start behavior

### 6.1 Explicit path

Canonical form:

```sh
temote session start temote --path src/temote-mcp
```

or current compatibility spelling:

```sh
temote-mcp session start temote --path src/temote-mcp
```

Resolution:

```text
named root "src"
    +
relative path "temote-mcp"
    ->
canonical physical cwd
```

All containment and symlink checks happen here.

### 6.2 Cwd shorthand

The convenience form may remain:

```sh
cd /Volumes/devstorage/Developer/temote-mcp
temote-mcp start temote
```

but cwd is no longer an independent authority source.

Temote must reverse-resolve cwd into a configured named root:

```text
/Volumes/devstorage/Developer/temote-mcp
        |
        v
src/temote-mcp
```

The resulting managed session is identical to an explicit named-root-relative start.

### 6.3 Unregistered cwd

If cwd is not inside a configured named root:

```text
error: current directory is outside all configured Temote roots

current:
  /tmp/example

configure a root or start from an existing named root
```

Do not automatically synthesize:

- `cwd=/tmp/example` as an implicit root
- HOME as a root
- `/` as a root
- repository top-level as a root

### 6.4 Ambiguous cwd

If nested or overlapping roots allow one physical cwd to reverse-resolve to multiple logical identities, do not silently pick whichever root happens to be iterated first.

Either:

1. reject overlapping roots at configuration time, or
2. define a deterministic explicit rule such as longest canonical root match

The chosen rule must be documented and tested.

Preferred direction: **longest containing root wins only if that rule is part of the public contract; otherwise fail on ambiguity**.

## 7. Root identity across Hosts

Root names should be treated as logical namespace labels, not derived machine paths.

Preferred model:

```text
Host A:
  src -> /Volumes/devstorage/Developer

Host B:
  src -> /home/user/src
```

This does not mean all content under both roots is identical.

Repository/workspace identity still needs its own contract.

Named root answers:

> which administrator-approved filesystem namespace contains this workspace on this Host?

Repository identity answers:

> which repository/project is this?

Workspace identity answers:

> which concrete working copy / branch / worktree state is this?

Do not collapse these into one identifier.

## 8. Relationship to repository/workspace contract

The named-root requirement should compose with the repository store / workspace design.

Conceptually:

```text
Host
  |
  +-- named root: src
        |
        +-- logical path: src/temote-mcp
              |
              +-- repository identity: github:f4ah6o/temote-mcp
              |
              +-- workspace identity: local working copy / worktree identity
```

The named root is the filesystem admission boundary.

It is not itself sufficient to establish repository identity.

## 9. Relationship to Temote Fabric

Fabric should not need to persist or route primarily by raw local absolute paths.

Useful shared coordinates are:

```text
host_id
root_name
root_relative_path
repository_id
workspace_id
session_id
task_id
execution_id
```

When a Host is offline, Fabric may retain the logical coordinates and last observed physical-path metadata, but the physical path is never globally authoritative.

This also makes mount relocation survivable when the Host root mapping changes.

## 10. Configuration direction

Keep current support:

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
```

and JSON form:

```sh
export TEMOTE_MCP_ROOTS='{"src":"~/src","work":"~/work"}'
```

But introduce an internal abstraction roughly equivalent to:

```text
RootRegistry
  get(name)
  resolve(logical_path)
  reverse_resolve(physical_path)
  validate()
```

Session lifecycle code should depend on `RootRegistry`, not environment parsing.

Environment parsing becomes one adapter.

## 11. Future CLI direction

A persistent UX can later provide:

```sh
temote root add src ~/src
temote root list
temote root show src
temote root remove src
```

Potential output:

```text
NAME   TARGET
src    /Volumes/devstorage/Developer
work   /Volumes/devstorage/Work
```

This issue does not require implementing persistent config immediately.

The important part is to avoid hard-wiring session invariants directly to `TEMOTE_MCP_ROOTS`.

## 12. Migration

Do not abruptly break existing local workflows.

### Phase A — normalize internally

- introduce one `RootRegistry`
- route remote explicit paths through it
- add reverse resolution for cwd compatibility
- store logical root + relative path in new session metadata

### Phase B — compatibility warning

For:

```sh
temote-mcp start temote
```

when cwd successfully reverse-resolves, allow it but warn that the command is being normalized to:

```text
src/temote-mcp
```

No warning is necessary forever; this phase exists to expose unexpected mappings.

### Phase C — hard fail outside roots

Once reverse resolution is proven:

- cwd inside a configured root -> valid
- cwd outside every configured root -> hard error
- no root registry -> hard error for managed session creation

### Phase D — persisted-session migration

Existing lifecycle metadata that contains only a physical cwd must be handled explicitly.

For each old session:

1. reverse-resolve the physical cwd
2. if exactly one valid mapping exists, persist the logical identity
3. if no safe mapping exists, leave the record inspectable but require explicit root configuration before restart
4. never silently broaden the root registry

## 13. Failure behavior

### Missing root registry

Fail session creation with setup guidance.

Do not start first and warn later.

### Missing root target

Fail closed.

A configured logical root whose physical target no longer exists must not silently fall back to another path.

### Symlink escape

Keep current canonicalization / containment behavior.

Logical descendant resolution must not escape the canonical physical root.

### Root mapping changed

A stopped session's logical identity may resolve to a new physical target after an intentional root mapping change.

Before restart, Temote must surface enough information to make that mapping observable.

For active sessions, root config changes must not teleport a running process to another cwd.

## 14. Security boundary

Named roots are not merely convenience aliases.

They are part of the host-side filesystem admission policy.

Therefore:

- root target canonicalization is required
- traversal escape is rejected
- descendant symlink escape is rejected
- remote callers cannot create new roots
- remote callers cannot pass arbitrary absolute cwd
- Fabric cannot grant a Host filesystem root
- backend agents inherit the already-resolved workspace boundary

## 15. Implementation packets

### NR0 — root registry abstraction

- [ ] introduce one internal root-registry abstraction
- [ ] move `TEMOTE_MCP_ROOTS` parsing behind it
- [ ] validate names/targets/canonicalization in one place
- [ ] preserve current managed-session behavior

### NR1 — reverse resolution

- [ ] add physical cwd -> logical named-root-relative resolution
- [ ] define nested/overlapping-root semantics
- [ ] test symlink/canonical path cases
- [ ] make compatibility `start <id>` use it

### NR2 — session metadata identity

- [ ] persist `root_name`
- [ ] persist `root_relative_path`
- [ ] keep canonical physical cwd as host-local runtime/diagnostic data
- [ ] migrate old physical-only records safely

### NR3 — enforce all entry points

- [ ] local CLI
- [ ] stdio MCP where session creation is available
- [ ] direct HTTP MCP
- [ ] Fabric-routed session creation
- [ ] restart / restore / upgrade paths
- [ ] automatic restart paths

All must use the same logical workspace resolver.

### NR4 — UX/docs

- [ ] actionable missing-root errors
- [ ] README updated
- [ ] `docs/managed-sessions.md` updated
- [ ] compatibility/deprecation behavior documented
- [ ] future persistent-root config contract documented if implemented

## 16. Acceptance criteria

- [ ] Every newly created managed session has a named root.
- [ ] Every newly created managed session has a root-relative logical path.
- [ ] Local and remote session creation use the same root resolver.
- [ ] `temote-mcp start <id>` cannot create an unregistered absolute/cwd workspace.
- [ ] Cwd shorthand reverse-resolves into a named root when valid.
- [ ] Missing root configuration fails closed.
- [ ] Missing root target fails closed.
- [ ] No HOME, `/`, repository-only, or arbitrary cwd fallback exists.
- [ ] Remote callers cannot register or broaden roots.
- [ ] Session metadata can distinguish logical workspace identity from physical cwd.
- [ ] Persisted legacy sessions have a safe migration path.
- [ ] Nested/overlapping root behavior is deterministic and tested.
- [ ] Host root remapping does not mutate active session cwd.
- [ ] Fabric/shared metadata does not treat host absolute paths as global identity.
- [ ] `TEMOTE_MCP_ROOTS` remains supported but is not the only possible configuration source.
- [ ] Documentation describes one consistent workspace identity model.

## 17. Non-goals

- Requiring all Hosts to use the same physical filesystem path.
- Treating root name as repository identity.
- Removing `TEMOTE_MCP_ROOTS`.
- Automatically discovering or authorizing arbitrary repositories.
- Letting remote callers create filesystem roots.
- Solving repository/worktree identity entirely in this issue.

## 18. Principle

> Require the named-root boundary, not the environment variable.

And:

> A Temote workspace should have a logical address before it has a host-specific path.
