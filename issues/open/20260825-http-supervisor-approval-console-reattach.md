# HTTP supervisor approval console reattachment

Date: 2026-08-25

## Context

The local lifecycle supervisor now treats `temote-mcp session console` as a reconnectable attachment. Console EOF/PTY disconnect leaves session runtimes alive and approval-required operations fail closed until a console reconnects.

The existing `temote-mcp serve` / `temote-mcp up` path also leaves managed runtimes alive when its stdin approval-console task exits: the HTTP server does not select on that task. Dropping the approval receiver causes subsequent approval-required operations to fail closed. However, that foreground stdin console cannot currently be reattached without restarting the HTTP supervisor process.

## Goal

Give the HTTP supervisor the same reconnectable approval-console model without making the console the runtime owner.

## Constraints

- do not expose approval attachment over the public HTTP MCP endpoint
- use a same-user local IPC boundary with private filesystem permissions
- preserve existing Cloudflare/Tailscale/OpenAI authentication and ingress behavior
- no yolo/session permission widening
- disconnect must deny any in-flight approval and leave runtimes alive
- subsequent local console attachment must service new approval requests
- avoid conflicting ownership if `temote-mcp supervisor` and `temote-mcp up` run concurrently; define distinct supervisor identity/socket addressing or converge ownership explicitly

## Acceptance tests

- stdin EOF on the original `serve` console does not stop HTTP or session runtimes
- approvals after disconnect fail closed
- a local console can reattach without restarting `serve`
- approval routing identifies the target session
- public clients cannot attach or approve through HTTP
- reconnecting one console does not alter sandbox/permission state
