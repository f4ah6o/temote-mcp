# local-mcp Cloudflare gateway

[日本語](README.ja.md)

This Worker exposes one MCP endpoint to ChatGPT and routes each tool call to a
Mac, Linux, or Windows/WSL2 host selected by `session_id`. Hosts make outbound
HTTPS long-poll requests; no inbound port or per-host Tunnel is required.

## Components

- `GatewaySession`: one Durable Object per `session_id`; owns the current host
  generation, request queue, pending responses, and host lease.
- `GatewayRegistry`: one Durable Object containing active session leases for
  `session_list`.
- Worker `/mcp`: validates Cloudflare Access assertions, serves MCP
  `initialize` and `tools/list`, and routes `tools/call`.
- Worker `/v1/hosts/*`: bearer-token-protected API used by
  `local-mcp gateway-agent`.

A reconnect increments the Durable Object generation. Requests and responses
from an older generation or process `instance_id` are rejected with HTTP 409.
The gateway does not automatically retry a timed-out tool call because tools
may be non-idempotent.

## Configure and deploy

1. Edit the non-secret Access values in `wrangler.toml`:

   - `ACCESS_TEAM_DOMAIN`
   - `ACCESS_AUDIENCE`
   - `ACCESS_ALLOWED_EMAILS`

2. Create a high-entropy host token and save it as a Worker secret:

       cd gateway
       npx wrangler secret put HOST_TOKEN

   `CLIENT_TOKEN` is an optional local-development fallback. Production
   ChatGPT traffic should use a verified `Cf-Access-Jwt-Assertion` instead.

3. Deploy the Worker and Durable Object exports configuration:

       npm test
       npx wrangler deploy --dry-run
       npx wrangler deploy

4. Attach a custom domain and protect it with a Cloudflare Access self-hosted
   application. Enable Managed OAuth for the ChatGPT MCP connection. Keep
   `workers_dev = false` so the Access-protected custom hostname is the public
   route.

5. Permit host agents through Access using a service-token policy. Store the
   service-token client ID and secret only on each endpoint. The Worker still
   requires `HOST_TOKEN`, so the Access service token is not the host protocol
   credential.

The public MCP URL is:

    https://<gateway-host>/mcp

## Start endpoint agents

Start the local approval/session UI from each project directory:

    local-mcp start mac-main

After approving `gateway_connect` in that terminal, start the outbound agent:

    export LOCAL_MCP_GATEWAY_URL=https://<gateway-host>
    export LOCAL_MCP_GATEWAY_HOST_TOKEN='<worker HOST_TOKEN>'
    export LOCAL_MCP_GATEWAY_ACCESS_CLIENT_ID='<Access service-token ID>'
    export LOCAL_MCP_GATEWAY_ACCESS_CLIENT_SECRET='<Access service-token secret>'
    local-mcp gateway-agent --session-id mac-main

Use a different ID on the Windows endpoint. Until native Windows transport and
sandbox support are implemented, run both commands inside WSL2:

    local-mcp start windows-wsl2-main
    local-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2

`--platform auto` detects macOS, ordinary Linux, and WSL2. A session ID is a
routing key, not a credential; endpoint approval, Access policy, and the host
token remain mandatory.

## Operational behavior

- Agent connect: increments generation and replaces the previous host for the
  same `session_id`.
- Agent poll: refreshes a 90-second lease and waits up to 20 seconds for work.
- Tool dispatch: waits up to 35 seconds for the endpoint response.
- Disconnect or lease expiry: pending calls fail; they are not replayed.
- Worker upgrade during an active call: the caller receives a retryable gateway
  error, but the gateway does not repeat the endpoint operation.
- `session_list`: reports Registry leases; final online validation still occurs
  in the per-session Durable Object.

For local Worker development, copy `.dev.vars.example` to `.dev.vars`. Never
commit `.dev.vars`, Worker secrets, Access service-token secrets, or endpoint
environment files.
