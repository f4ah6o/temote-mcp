# Temote MCP Cloudflare gateway

任意機能の Worker は、1つの公開 MCP endpoint から `session_id` ごとに複数の Temote MCP host session へ Durable Objects + outbound HTTPS long polling で route します。

設定、deploy、endpoint agent、運用の詳細は [`../docs/gateway.ja.md`](../docs/gateway.ja.md) を参照してください。

最小の検証:

```sh
npm test
npx wrangler deploy --dry-run
```
