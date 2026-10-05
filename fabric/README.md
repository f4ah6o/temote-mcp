# Temote Fabric

The optional Worker routes one public MCP endpoint to multiple Temote MCP host sessions by `session_id` using Durable Objects and outbound HTTPS long polling.

See the full setup, deployment, endpoint-agent, and operational reference in [`../docs/gateway.md`](../docs/gateway.md).

Quick validation:

```sh
npm test
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf build
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf deploy --prebuilt --dry-run
```
