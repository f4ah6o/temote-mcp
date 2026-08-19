# Multi-host Cloudflare gateway

[日本語](gateway.ja.md)

The optional `gateway/` Worker exposes one MCP endpoint and routes calls to Mac, Linux, or Windows/WSL2 hosts by `session_id`. Host agents use outbound HTTPS long polling, so endpoints do not need inbound ports or per-host Tunnels.

## Components

- `GatewaySession`: one Durable Object per `session_id`, holding generation, request queue, pending responses, and the current host lease.
- `GatewayRegistry`: tracks active session leases for `session_list`.
- Worker `/mcp`: validates Cloudflare Access assertions, serves MCP initialization/tool listing, and forwards calls to the selected `GatewaySession`.
- `/v1/hosts/*`: bearer-token-protected protocol used by `temote-mcp gateway-agent`.

A reconnect increments the host generation. Requests/responses from an older generation or process `instance_id` are rejected. Timed-out tool calls are not automatically replayed because some operations are non-idempotent.

## MCP protocol compatibility

The gateway serves both MCP `2026-07-28` and the existing 2025-era handshake. Modern requests are validated for per-request `_meta` and the standard HTTP routing headers before dispatch. Modern `server/discover`, `tools/list`, and tool results include the required 2026 result metadata while legacy `initialize` behavior remains unchanged.

## Deploy

1. Set non-secret Access values in `gateway/wrangler.toml`: `ACCESS_TEAM_DOMAIN`, `ACCESS_AUDIENCE`, and `ACCESS_ALLOWED_EMAILS`.
2. Store a high-entropy host token as the Worker secret `HOST_TOKEN`.
3. Run tests and a dry-run before deployment:

```sh
cd gateway
npm test
npx wrangler deploy --dry-run
npx wrangler deploy
```

4. Attach a custom domain, protect it with a self-hosted Cloudflare Access application, and enable Managed OAuth for the intended MCP clients. Keep `workers_dev = false` so the Access-protected custom hostname is the public route.
5. Allow host agents through Access using a service-token policy. The Access service token and `HOST_TOKEN` are separate credentials.

The public URL is `https://<gateway-host>/mcp`.

## Endpoint agents

Start a local session first:

```sh
temote-mcp start mac-main
```

Approve `gateway_connect` in that session terminal, then run:

```sh
export TEMOTE_MCP_GATEWAY_URL=https://<gateway-host>
export TEMOTE_MCP_GATEWAY_HOST_TOKEN='<worker HOST_TOKEN>'
export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID='<Access service-token ID>'
export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET='<Access service-token secret>'
temote-mcp gateway-agent --session-id mac-main
```

Until native Windows transport/sandbox support exists, use WSL2:

```sh
temote-mcp start windows-wsl2-main
temote-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2
```

`--platform auto` distinguishes macOS, Linux, and WSL2. A session ID is a routing key, not a credential.

## Operational behavior

- Each poll refreshes a 90-second lease and waits up to 20 seconds for work.
- Gateway tool dispatch waits up to 35 seconds for the endpoint response.
- If a host disconnects or its lease expires, pending calls fail and are not replayed.
- Worker replacement during a call can return a retryable gateway error, but the endpoint operation is not repeated automatically.
- `session_list` uses registry leases; each per-session Durable Object still performs the final online check.

For local Worker development, copy `gateway/.dev.vars.example` to `gateway/.dev.vars`. Never commit `.dev.vars`, Worker secrets, Access service-token secrets, or endpoint environment files.
