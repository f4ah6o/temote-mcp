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
                                        temote-mcp serve
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
- `TEMOTE_MCP_TUNNEL_TOKEN`

`just env-check` validates presence without printing secret values.

## Run

Use `just up` to build and run the origin plus Tunnel together, or run them separately:

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
cloudflared tunnel run --token "$TEMOTE_MCP_TUNNEL_TOKEN"
```

Local project sessions are separate processes and must also be running.

## Cloudflare Access

For a route such as `https://temotemcp.example.com/mcp`:

1. Route a remotely managed Tunnel hostname to `http://127.0.0.1:8791`.
2. Protect the entire hostname with a self-hosted Cloudflare Access application. Do not scope the application only to `/mcp`, because Managed OAuth discovery uses host-root `/.well-known/` paths.
3. Add an Allow policy for the intended identity set. Do not use Bypass for the public MCP route.
4. Enable Managed OAuth for the intended MCP/OAuth clients. Configure dynamic client registration, token/grant lifetimes, redirect URIs, and loopback options according to those clients.
5. Put the self-hosted application's `AUD` in `TEMOTE_MCP_ACCESS_AUDIENCE`.

A Cloudflare `AI controls > MCP servers` portal registration is separate from the self-hosted Access application that protects the hostname.

The Rust origin validates the forwarded `Cf-Access-Jwt-Assertion` signature, issuer, audience, expiry, subject, and configured email allow list.

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
