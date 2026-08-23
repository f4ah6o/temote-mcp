# Managed sessions and named roots

`temote-mcp serve` can supervise multiple normal sessions created by an authenticated direct HTTP MCP client.

## Named-root configuration

`TEMOTE_MCP_ROOTS` separates the MCP logical namespace from host filesystem paths.

For one root:

```sh
TEMOTE_MCP_ROOTS='src=~/src'
```

For multiple roots, use a JSON object rather than a separator-based list:

```sh
TEMOTE_MCP_ROOTS='{"src":"~/src","work":"~/work"}'
```

Root names accept only ASCII letters, digits, `-`, and `_`. The configured root path is canonicalized first. This intentionally allows an administrator-selected root alias such as:

```text
~/src -> /Volumes/devstorage/Developer
```

The canonical target becomes the physical containment boundary. `src/foo` is joined under that boundary, canonicalized, required to be a directory, and then checked to be the root itself or a descendant. A descendant symlink or `..` traversal that resolves outside the physical root is rejected. Missing root configuration fails closed; there is no fallback to HOME, `/`, process cwd, or repository cwd.

## Runtime and supervisor

The session socket remains the runtime boundary for MCP operations and host bridges. CLI and managed sessions both use the same reusable session runtime for metadata, Unix socket lifecycle, approval state, 1Password/kintone bridge state, and cleanup.

`temote-mcp start` wraps one runtime with the traditional single-session terminal UI. `temote-mcp serve` owns a `SessionSupervisor` and one shared local approval console. Approval prompts include the target `session_id`, cwd, and operation before a local `y/yes` or `n/no` response is accepted.

Managed sessions are always created with `yolo=false`. The MCP `session_start` schema has no yolo option, and extra input fields are rejected. Git network operations, 1Password service-account commands, kintone calls, and other existing approval-gated host operations therefore continue to wait for local approval.

## Ownership and lifecycle

The supervisor tracks only runtimes it created. `session_stop` removes and gracefully shuts down only such a runtime. An independently running CLI session can still appear in `session_list`, but a remote client cannot stop it through `session_stop`.

Active session-ID collisions fail instead of replacing the existing socket/runtime. When `serve` terminates, it drains all managed runtime handles, marks their metadata inactive (`process_id = 0`), and removes their Unix sockets.

`temote-mcp up` keeps the HTTP server as the foreground process and owns only the selected connection child (`cloudflared`, `tailscale funnel`, or `tunnel-client`). This gives the approval console the terminal stdin and ties Temote-owned connection cleanup to the same shutdown path without stopping the Tailscale daemon or unrelated ingress/tunnel configuration. `temote-mcp down` requests the same graceful shutdown and removes stale lifecycle state.

## Endpoint scope

`session_start` and `session_stop` are exposed only by the authenticated direct HTTP `serve` endpoint. The Cloudflare Workers + Durable Objects multi-host gateway does not advertise them and has no host-selection contract in this design. The existing public exclusion of `without_sandbox` remains unchanged.
