# Multi-host Cloudflare gateway

[English](gateway.md)

任意機能の `gateway/` Worker は1つの MCP endpoint を公開し、`session_id` ごとに Mac、Linux、Windows/WSL2 host へ call を振り分けます。host agent は outbound HTTPS long poll を使うため、endpoint ごとの inbound port や Tunnel は不要です。

## 構成

- `GatewaySession`: `session_id` ごとの Durable Object。generation、request queue、pending response、host lease を保持
- `GatewayRegistry`: `session_list` 用の active lease を集約
- Worker `/mcp`: Cloudflare Access assertion を検証し、MCP initialize/tool list に応答し、call を対象 `GatewaySession` へ転送
- `/v1/hosts/*`: `temote-mcp gateway-agent` が使う bearer-token-protected protocol

host reconnect 時は generation を増やし、古い generation や process `instance_id` からの request/response を拒否します。非 idempotent operation があるため timeout 後の tool call は自動 replay しません。

## deploy

1. `gateway/wrangler.toml` に non-secret の `ACCESS_TEAM_DOMAIN`、`ACCESS_AUDIENCE`、`ACCESS_ALLOWED_EMAILS` を設定します。
2. 十分に強い host token を Worker secret `HOST_TOKEN` として保存します。
3. test と dry-run 後に deploy します。

```sh
cd gateway
npm test
npx wrangler deploy --dry-run
npx wrangler deploy
```

4. custom domain を割り当て、self-hosted Cloudflare Access application で保護し、利用する MCP client 向けに Managed OAuth を有効化します。Access 配下の custom hostname だけを公開するため `workers_dev = false` を維持します。
5. host agent は Access service-token policy で許可します。Access service token と `HOST_TOKEN` は別 credential です。

公開 URL は `https://<gateway-host>/mcp` です。

## endpoint agent

先に local session を起動します。

```sh
temote-mcp start mac-main
```

その session terminal で `gateway_connect` を承認し、agent を起動します。

```sh
export TEMOTE_MCP_GATEWAY_URL=https://<gateway-host>
export TEMOTE_MCP_GATEWAY_HOST_TOKEN='<worker HOST_TOKEN>'
export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID='<Access service-token ID>'
export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET='<Access service-token secret>'
temote-mcp gateway-agent --session-id mac-main
```

native Windows transport/sandbox が入るまでは WSL2 を使います。

```sh
temote-mcp start windows-wsl2-main
temote-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2
```

`--platform auto` は macOS、Linux、WSL2 を判別します。session ID は routing key であり credential ではありません。

## 運用

- poll ごとに90秒 lease を更新し、最大20秒 work を待機
- gateway dispatch は endpoint response を最大35秒待機
- host disconnect / lease expiry 時は pending call を失敗させ、自動 replay しない
- call 中の Worker replacement は retryable gateway error になり得るが、endpoint operation 自体は自動再実行しない
- `session_list` は Registry lease を使い、最終 online check は per-session Durable Object でも行う

local Worker 開発では `gateway/.dev.vars.example` を `gateway/.dev.vars` にコピーします。`.dev.vars`、Worker secret、Access service-token secret、endpoint environment file は commit しないでください。
