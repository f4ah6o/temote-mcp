# Public HTTP endpoint

[日本語](public-http.ja.md)

Temote MCP can expose `/mcp` through an on-demand Cloudflare Tunnel protected by Cloudflare Access.

```text
MCP client
    | OAuth
    v
Cloudflare Access -- Cloudflare Tunnel -- 127.0.0.1:8791
                                               |
                                               v
                                        temote-mcp up
```

## Environment

Keep the deployment environment outside the repository and mode 0600:

```sh
install -d -m 700 ~/.config/temote-mcp
cp .env.example ~/.config/temote-mcp/public.env
chmod 600 ~/.config/temote-mcp/public.env
```

Required values are:

- `TEMOTE_MCP_PUBLIC_URL`
- `TEMOTE_MCP_ACCESS_TEAM_DOMAIN`
- `TEMOTE_MCP_ACCESS_AUDIENCE`
- `TEMOTE_MCP_ACCESS_ALLOWED_EMAILS`
- `~/.config/temote-mcp/tunnel-token` (mode `0600`; override with `TUNNEL_TOKEN_FILE`)

`temote-mcp up` loads this file and validates the runtime configuration without requiring `just`. In a repository checkout, `just env-check` remains a development-only preflight that does not print secret values.

For local Tunnel diagnostics, `temote-mcp doctor` checks `cloudflared` and the token file. Add `--cloudflare` to query the configured Tunnel status through the Cloudflare API; this requires the account ID, Tunnel ID, and API token environment variables documented in [development diagnostics](development.md).

## Run

Use `temote-mcp up` to run the origin and Tunnel together. Stop it with `temote-mcp down`. In a repository checkout, `just up/down` are development wrappers. To run them separately:

```sh
set -a
. ~/.config/temote-mcp/public.env
set +a
temote-mcp serve
```

```sh
set -a
. ~/.config/temote-mcp/public.env
set +a
cloudflared tunnel run --token-file "${TUNNEL_TOKEN_FILE:-$HOME/.config/temote-mcp/tunnel-token}"
```

With `TEMOTE_MCP_ROOTS` configured, the direct HTTP client can create managed project sessions with `session_start`. Separately started CLI sessions remain supported.

## Cloudflare Access

For a route such as `https://temotemcp.example.com/mcp`:

1. Route a remotely managed Tunnel hostname to `http://127.0.0.1:8791`.
2. Protect the entire hostname with a self-hosted Cloudflare Access application. Do not scope the application only to `/mcp`, because Managed OAuth discovery uses host-root `/.well-known/` paths.
3. Add an Allow policy for the intended identity set. Do not use Bypass for the public MCP route.
4. Enable Managed OAuth for the intended MCP/OAuth clients. Configure dynamic client registration, token/grant lifetimes, redirect URIs, and loopback options according to those clients.
5. Put the self-hosted application's `AUD` in `TEMOTE_MCP_ACCESS_AUDIENCE`.

A Cloudflare `AI controls > MCP servers` portal registration is separate from the self-hosted Access application that protects the hostname.

The Rust origin validates the forwarded `Cf-Access-Jwt-Assertion` signature, issuer, audience, expiry, subject, and configured email allow list.

## MCP protocol compatibility

The public endpoint supports both MCP `2026-07-28` and the existing 2025-era handshake. Modern requests use `server/discover`, per-request `_meta`, and the `MCP-Protocol-Version` / `Mcp-Method` / `Mcp-Name` HTTP headers. Legacy clients continue to use `initialize`; no `Mcp-Session-Id` is created for modern requests.

## Probe

Before attaching an MCP client, verify that Access intercepts the origin:

```sh
curl -i -X POST https://temotemcp.example.com/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'

curl -i https://temotemcp.example.com/.well-known/oauth-authorization-server
curl -i https://temotemcp.example.com/.well-known/oauth-protected-resource
```

Expected unauthenticated behavior is a Cloudflare `401` with `WWW-Authenticate` for `/mcp` and JSON metadata from the discovery endpoints. A `530` normally means the Tunnel or origin is not running. A Rust JSON `401` without Cloudflare's challenge, or discovery `404`, indicates the Access application is not protecting the expected hostname/path.

If the origin reports an invalid Access audience, copy the `AUD` from the hostname-protecting self-hosted application and restart `temote-mcp serve`.

## Public tool boundary

Public HTTP uses the same session model as local stdio. It does not expose `without_sandbox`. A session explicitly started with `--yolo` still gives its ordinary public command tools unrestricted host permissions, so Cloudflare Access is an authentication boundary, not a replacement for session-mode decisions.

## Managed session lifecycle

When `TEMOTE_MCP_ROOTS` is configured, the authenticated direct HTTP endpoint exposes `session_start` and `session_stop`. `session_start` accepts only logical named-root-relative paths and has no yolo option. Absolute paths, unknown roots, traversal/symlink escapes, and roots-unset fallback are rejected. `session_stop` is limited to sessions owned by the current `serve` process. `without_sandbox` remains unavailable on the public endpoint.

`temote-mcp up` keeps `temote-mcp serve` in the foreground and runs `cloudflared` as its child. The local approval console therefore owns stdin, and shutdown cleans up managed sessions and the Tunnel together. Stop it with `temote-mcp down`.
