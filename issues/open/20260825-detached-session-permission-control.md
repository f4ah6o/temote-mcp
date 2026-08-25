# Detached session permission control parity

Date: 2026-08-25

## Context

Before session runtimes were detached from their terminal, `temote-mcp start` exposed inline commands such as:

```text
/permission ask
/permission yolo
/permission allow <directory>
/permission revoke <directory>
/permission list
/permission status
```

The lifecycle-supervisor change intentionally separates runtime ownership from the approval console. The initial local supervisor control surface keeps the runtime's persisted permission roots and fail-closed behavior, but does not yet expose mutation of those permissions through `temote-mcp session ...`.

This is a UX parity gap, not a widening of permissions.

## Goal

Provide explicit detached-session permission management without making the approval console the runtime owner.

Possible CLI shape:

```text
temote-mcp session permission <id> status
temote-mcp session permission <id> allow <directory>
temote-mcp session permission <id> revoke <directory>
temote-mcp session permission <id> ask
temote-mcp session permission <id> yolo
```

## Security requirements

- permission mutations are available only over the same-user local supervisor control socket unless a separate authenticated remote policy is explicitly designed
- remote MCP `session_start` remains unable to create yolo sessions
- no implicit promotion from normal sandboxed mode to yolo
- canonical-path and symlink containment checks remain unchanged
- session cwd cannot be revoked
- approval-required transitions must fail closed when their local authorization path is unavailable
- permission changes should be observable/auditable in lifecycle or activity output
- no permission change may restart the runtime in a way that silently drops sandbox state

## Acceptance tests

- status/list reports the persisted roots and mode
- allow/revoke preserves canonical containment rules
- cwd revoke is rejected
- yolo transition is local-only and explicit
- remote MCP cannot invoke the local-only control path
- console disconnect does not change permissions
- supervisor restart preserves the resulting permission state
