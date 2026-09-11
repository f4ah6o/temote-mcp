# Default `agent` permission mode — sandboxed, approval-free agent operation

## Status

Design issue. Not implemented.

## Problem

Temote currently has an overly binary permission model for day-to-day remote development:

- `ask` preserves the normal sandbox/path boundary, but `git_fetch`, `git_pull`, `git_push`, and `local_agent_run` require local approval.
- `yolo` removes those approval prompts, but also removes the normal filesystem/sandbox/network restrictions and is intentionally unavailable to public MCP clients.

For the normal ChatGPT -> public MCP -> Temote workflow, this creates unnecessary friction. Git and local-agent operations are already exposed through structured, narrower tools, but they are blocked whenever the local approval console is unavailable. Switching the entire session to `yolo` would grant much more authority than is needed.

The desired default is:

> keep the normal Temote sandbox and directory containment, while removing the Temote local approval step for operations that are otherwise valid under the sandboxed/structured-tool contract.

## Goal

Introduce a third permission mode:

```text
ask | agent | yolo
```

and make `agent` the default for newly created sessions.

`agent` means **sandboxed but approval-free**. It is not a weaker spelling of `yolo`; the filesystem/path/network sandbox remains authoritative.

## Permission contract

### `ask`

Keep the existing strict behavior:

- normal filesystem/path containment
- ordinary `execute` / `start_command` remain sandboxed with network disabled
- host/network-sensitive structured operations may require local approval

This remains available as an explicit opt-in mode.

### `agent` — new default

Preserve the normal session boundary:

- `yolo=false`
- filesystem access remains limited to the configured session roots
- protected metadata rules remain in force
- ordinary `execute` / `start_command` remain sandboxed
- ordinary command network remains disabled
- public MCP may create/use this mode because it does not remove the sandbox boundary

Do not require Temote local approval for operations that are valid under the selected sandboxed/structured capability. In particular, the normal development path must work approval-free for:

1. `git_fetch` / `git_pull` / `git_push`
2. `local_agent_run`
3. ordinary sandboxed `execute` / `start_command`

The absence of approval does not widen filesystem, command, Git, network, secret, or tool-specific capability. Each operation must still satisfy its own sandbox/structured-tool policy.

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

`agent` removes the Temote local approval step for otherwise-valid operations, including structured integrations. It does not grant capabilities that the sandbox/structured tool contract does not already expose. Public `without_sandbox`, unrestricted host execution, force push, arbitrary Git refspecs, or path escapes remain unavailable. Secret-bearing integrations and external systems continue to obey their own authentication, authorization, argument-validation, secret-isolation, and capability contracts; `agent` must not be treated as credential escalation.

### `yolo`

Keep the current local-only unrestricted mode:

- no normal Temote sandbox/path restriction
- host filesystem/process/network permissions of the local user
- Temote approval prompts bypassed according to the existing yolo contract

Public MCP must still be unable to create or promote a session to `yolo`.

## Default behavior

Change the default for newly created sessions from `ask` to `agent`:

- local managed session start defaults to `agent`
- authenticated public `session_start` defaults to `agent`
- explicit `ask` remains available for stricter sessions
- explicit `yolo` remains local-only

Existing persisted/running sessions should keep their stored permission mode across upgrade/restart; do not silently rewrite an existing `ask` session to `agent` during restoration.

Provide an explicit local permission transition for existing sessions, conceptually:

```text
temote-mcp session permission <session-id> agent
```

## Implementation direction

Do not encode this as another special case around the current `yolo: bool` model.

Make permission mode an explicit enum/source of truth, conceptually:

```text
PermissionMode::Ask
PermissionMode::Agent
PermissionMode::Yolo
```

A compatibility `yolo` boolean may still be exposed where necessary, but authorization decisions should derive from the permission mode instead of accumulating boolean exceptions.

Tool authorization should be policy-driven, for example:

```text
operation             ask          agent         yolo
---------------------------------------------------------
git_fetch             approve      allow         allow
git_pull              approve      allow         allow
git_push              approve      allow         allow
local_agent_run       approve      allow         allow
ordinary execute      sandbox      sandbox       host
1Password secrets     approve      allow*        allow*
kintone mutation      approve      allow*        allow*
```

`*` subject to each integration's own authentication/authorization/capability contract. `agent` skips only the Temote local approval prompt; it does not bypass the upstream system's controls.

## Public MCP contract

`agent` must work through the public MCP endpoint without relying on the public-yolo exception proposed elsewhere.

The public boundary remains:

- remote client cannot request/create `yolo`
- remote client cannot self-promote to `yolo`
- `without_sandbox` remains absent from the public surface
- `agent` does not grant arbitrary host execution

This should eliminate the current deadlock where `ask` requires an attached local approval console but `yolo` is rejected by the public endpoint.

## Required tests

- [ ] Newly created local managed sessions default to `permission_mode=agent`.
- [ ] Newly created public managed sessions default to `permission_mode=agent` and `yolo=false`.
- [ ] Explicit `ask` sessions keep existing approval behavior.
- [ ] Explicit local `yolo` sessions keep existing unrestricted behavior.
- [ ] `agent` `git_fetch` runs without local approval and preserves safe-remote/refspec restrictions.
- [ ] `agent` `git_pull` runs without local approval and remains `--ff-only`.
- [ ] `agent` `git_push` runs without local approval and cannot force push.
- [ ] `agent` `local_agent_run` runs without local approval through the public MCP endpoint.
- [ ] `agent` ordinary `execute` remains sandboxed and network-disabled.
- [ ] `agent` cannot use public `without_sandbox`.
- [ ] `agent` does not invoke the Temote local approval console for otherwise-valid structured integrations; each integration still enforces its own authentication, authorization, argument validation, secret isolation, and capability contract.
- [ ] Permission mode survives lifecycle supervisor restart/restore and upgrade handoff.
- [ ] Existing stored `ask` sessions are not silently migrated to `agent`.
- [ ] Session list/info and docs expose `ask | agent | yolo` consistently.
- [ ] Existing Git, sandbox, approval, local-agent, public HTTP, gateway, and lifecycle tests remain green.

## Acceptance criteria

- [ ] A normal ChatGPT/public-MCP agent session can run sandboxed commands, fetch/pull/push, use structured integrations, and delegate to Codex/OpenCode without a Temote local approval console.
- [ ] The same session remains directory-contained and sandboxed; approval-free does not mean sandbox-free.
- [ ] No force-push or arbitrary Git/host-command capability is introduced.
- [ ] Public MCP does not gain yolo capability.
- [ ] `agent` is the default for new sessions; `ask` and local-only `yolo` remain explicit alternatives.
