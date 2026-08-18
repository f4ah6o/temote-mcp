# temote-mcp Cloudflare gateway

[日本語](README.ja.md)

This Worker exposes a single MCP endpoint to ChatGPT and routes each tool call to a Mac, Linux, or Windows/WSL2 host by `session_id`. Hosts connect with outbound HTTPS long polling, so they do not need an inbound port or a per-host Tunnel.

## Components

`GatewaySession` is a Durable Object created per `session_id`. It holds the current host generation, request queue, pending responses, and host lease.

`GatewayRegistry` tracks active session leases for `session_list`.

The Worker `/mcp` endpoint validates Cloudflare Access assertions, serves MCP `initialize` and `tools/list`, and forwards `tools/call` to the selected `GatewaySession`.

`/v1/hosts/*` is the bearer-token-protected API used by `temote-mcp gateway-agent`.

When a host reconnects, the Durable Object generation increases. Requests and responses from an older generation or process `instance_id` are rejected with HTTP 409. The gateway does not automatically retry timed-out tool calls because some tools are non-idempotent.

## Configure and deploy

1. Set the non-secret Access values in `wrangler.toml`:

   - `ACCESS_TEAM_DOMAIN`
   - `ACCESS_AUDIENCE`
   - `ACCESS_ALLOWED_EMAILS`

2. Generate a high-entropy host token and store it as a Worker secret:

       cd gateway
       npx wrangler secret put HOST_TOKEN

   `CLIENT_TOKEN` is only a local-development fallback. Production ChatGPT traffic should use a verified `Cf-Access-Jwt-Assertion`.

3. Test and dry-run the deployment before publishing it:

       npm test
       npx wrangler deploy --dry-run
       npx wrangler deploy

4. Attach a custom domain and protect it with a Cloudflare Access self-hosted application. Enable Managed OAuth for the ChatGPT MCP connection. Keep `workers_dev = false` so the Access-protected custom hostname remains the public route.

5. Allow host agents through Access with a service-token policy. Store the service-token client ID and secret only on each endpoint. The Worker also requires `HOST_TOKEN`, so the Access service token is separate from the host-protocol credential.

The public MCP URL has this form:

    https://<gateway-host>/mcp

## Start endpoint agents

Start a local session from the project directory first:

    temote-mcp start mac-main

Approve `gateway_connect` in that terminal, then start the outbound agent:

    export TEMOTE_MCP_GATEWAY_URL=https://<gateway-host>
    export TEMOTE_MCP_GATEWAY_HOST_TOKEN='<worker HOST_TOKEN>'
    export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID='<Access service-token ID>'
    export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET='<Access service-token secret>'
    temote-mcp gateway-agent --session-id mac-main

Use a different session ID for the Windows endpoint. Until native Windows transport and sandbox support are implemented, run both the session and the agent inside WSL2:

    temote-mcp start windows-wsl2-main
    temote-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2

`--platform auto` detects macOS, ordinary Linux, and WSL2. A session ID is a routing key, not a credential. Endpoint approval, the Access policy, and the host token are still required.

## Operational behavior

A host connection increments the generation and replaces the previous host for the same `session_id`.

Each agent poll refreshes a 90-second lease and waits up to 20 seconds for work. Tool dispatch waits up to 35 seconds for the endpoint response.

If the host disconnects or its lease expires, pending calls fail and are not replayed. If the Worker is upgraded during an active call, the caller receives a retryable gateway error, but the gateway does not repeat the endpoint operation.

`session_list` reports leases from `GatewayRegistry`. The per-session Durable Object still performs the final online check.

For local Worker development, copy `.dev.vars.example` to `.dev.vars`. Never commit `.dev.vars`, Worker secrets, Access service-token secrets, or endpoint environment files.
