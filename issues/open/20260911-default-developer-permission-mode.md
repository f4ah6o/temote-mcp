# Default `developer` permission mode — approval-free Git and local agents

## Status

Design issue. Not implemented.

## Problem

Temote currently has an overly binary permission model for day-to-day remote development:

- `ask` preserves the normal sandbox/path boundary, but `git_fetch`, `git_pull`, `git_push`, and `local_agent_run` require local approval.
- `yolo` removes those approval prompts, but also removes the normal filesystem/sandbox/network restrictions and is intentionally unavailable to public MCP clients.

For the normal ChatGPT -> public MCP -> Temote workflow, this creates unnecessary friction. Git and local-agent operations are already exposed through structured, narrower tools, but they are blocked whenever the local approval console is unavailable. Switching the entire session to `yolo` would grant much more authority than is needed.

The desired default is:

> keep the normal Temote sandbox and directory containment, while allowing the dedicated Git tools and `local_agent_run` to run without Temote local approval.

## Goal

Introduce a third permission mode:

```text
ask | developer | yolo
```

and make `developer` the default for newly created sessions.

`developer` is a normal sandboxed mode. It is **not** a weaker spelling of `yolo`.

## Permission contract

### `ask`

Keep the existing strict behavior:

- normal filesystem/path containment
- ordinary `execute` / `start_command` remain sandboxed with network disabled
- host/network-sensitive structured operations may require local approval

This remains available as an explicit opt-in mode.

### `developer` — new default

Preserve the normal session boundary:

- `yolo=false`
- filesystem access remains limited to the configured session roots
- protected metadata rules remain in force
- ordinary `execute` / `start_command` remain sandboxed
- ordinary command network remains disabled
- public MCP may create/use this mode because it does not remove the sandbox boundary

Skip Temote local approval only for these structured developer operations:

1. `git_fetch`
2. `git_pull`
3. `git_push`
4. `local_agent_run`

All existing tool-specific restrictions remain authoritative.

#### Git invariants

Approval-free Git must not become generic Git execution:

- `git_fetch` accepts only a configured safe remote; no arbitrary URL or caller-provided refspec
- `git_pull` remains `--ff-only`
- `git_push` remains current-branch push only
- force push remains unavailable
- hooks remain disabled
- cwd remains canonicalized inside the session roots

#### `local_agent_run` invariants

Approval-free local-agent execution must keep the existing broker contract:

- only the verified Codex/OpenCode adapters are selectable
- caller supplies a bounded task, not arbitrary executable/argv
- cwd remains canonicalized inside the session roots
- access stays limited to the existing `read_only` / `workspace_write` contract
- agent state/environment isolation remains in force
- bounded output/job ownership/cancellation remain unchanged
- Temote-held credentials are not implicitly inherited
- production-operation authorization is not bypassed

Other approval-gated host integrations such as secret-bearing 1Password operations, forwarded kintone mutations, generic host execution, or future unrelated privileged tools do **not** become approval-free merely because the session is `developer`.

### `yolo`

Keep the current local-only unrestricted mode:

- no normal Temote sandbox/path restriction
- host filesystem/process/network permissions of the local user
- Temote approval prompts bypassed according to the existing yolo contract

Public MCP must still be unable to create or promote a session to `yolo`.

## Default behavior

Change the default for newly created sessions from `ask` to `developer`:

- local managed session start defaults to `developer`
- authenticated public `session_start` defaults to `developer`
- explicit `ask` remains available for stricter sessions
- explicit `yolo` remains local-only

Existing persisted/running sessions should keep their stored permission mode across upgrade/restart; do not silently rewrite an existing `ask` session to `developer` during restoration.

Provide an explicit local permission transition for existing sessions, conceptually:

```text
temote-mcp session permission <session-id> developer
```

## Implementation direction

Do not encode this as another special case around the current `yolo: bool` model.

Make permission mode an explicit enum/source of truth, conceptually:

```text
PermissionMode::Ask
PermissionMode::Developer
PermissionMode::Yolo
```

A compatibility `yolo` boolean may still be exposed where necessary, but authorization decisions should derive from the permission mode instead of accumulating boolean exceptions.

Tool authorization should be policy-driven, for example:

```text
operation             ask          developer     yolo
---------------------------------------------------------
git_fetch             approve      allow         allow
git_pull              approve      allow         allow
git_push              approve      allow         allow
local_agent_run       approve      allow         allow
ordinary execute      sandbox      sandbox       host
1Password secrets     approve      approve       allow*
kintone mutation      approve      approve       allow*
```

`*` subject to each integration's existing yolo contract and any external authorization layer.

## Public MCP contract

`developer` must work through the public MCP endpoint without relying on the public-yolo exception proposed elsewhere.

The public boundary remains:

- remote client cannot request/create `yolo`
- remote client cannot self-promote to `yolo`
- `without_sandbox` remains absent from the public surface
- `developer` does not grant arbitrary host execution

This should eliminate the current deadlock where `ask` requires an attached local approval console but `yolo` is rejected by the public endpoint.

## Required tests

- [ ] Newly created local managed sessions default to `permission_mode=developer`.
- [ ] Newly created public managed sessions default to `permission_mode=developer` and `yolo=false`.
- [ ] Explicit `ask` sessions keep existing approval behavior.
- [ ] Explicit local `yolo` sessions keep existing unrestricted behavior.
- [ ] `developer` `git_fetch` runs without local approval and preserves safe-remote/refspec restrictions.
- [ ] `developer` `git_pull` runs without local approval and remains `--ff-only`.
- [ ] `developer` `git_push` runs without local approval and cannot force push.
- [ ] `developer` `local_agent_run` runs without local approval through the public MCP endpoint.
- [ ] `developer` ordinary `execute` remains sandboxed and network-disabled.
- [ ] `developer` cannot use public `without_sandbox`.
- [ ] Secret-bearing / unrelated privileged integrations remain approval-gated in `developer` unless separately designed otherwise.
- [ ] Permission mode survives lifecycle supervisor restart/restore and upgrade handoff.
- [ ] Existing stored `ask` sessions are not silently migrated to `developer`.
- [ ] Session list/info and docs expose `ask | developer | yolo` consistently.
- [ ] Existing Git, sandbox, approval, local-agent, public HTTP, gateway, and lifecycle tests remain green.

## Acceptance criteria

- [ ] A normal ChatGPT/public-MCP development session can fetch, pull, push, and delegate to Codex/OpenCode without a local approval console.
- [ ] The same session remains directory-contained and sandboxed for ordinary commands.
- [ ] No force-push or arbitrary Git/host-command capability is introduced.
- [ ] Public MCP does not gain yolo capability.
- [ ] `developer` is the default for new sessions; `ask` and local-only `yolo` remain explicit alternatives.
