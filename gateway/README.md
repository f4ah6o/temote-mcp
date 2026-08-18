# Temote MCP Cloudflare gateway

The optional Worker routes one public MCP endpoint to multiple Temote MCP host sessions by `session_id` using Durable Objects and outbound HTTPS long polling.

See the full setup, deployment, endpoint-agent, and operational reference in [`../docs/gateway.md`](../docs/gateway.md).

Quick validation:

```sh
npm test
npx wrangler deploy --dry-run
```
