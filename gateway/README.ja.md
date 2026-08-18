# local-mcp Cloudflare gateway

[English](README.md)

この Worker は ChatGPT に1つの MCP endpoint を公開し、各 tool call を `session_id` で選択した Mac、Linux、Windows/WSL2 host へ route します。host 側は outbound HTTPS long poll を行うため、inbound port や host ごとの Tunnel は不要です。

## Components

- `GatewaySession`: `session_id` ごとに1つの Durable Object。現在の host generation、request queue、pending response、host lease を保持します。
- `GatewayRegistry`: active session lease を保持し、`session_list` に使用する Durable Object です。
- Worker `/mcp`: Cloudflare Access assertion を検証し、MCP `initialize` / `tools/list` を提供し、`tools/call` を route します。
- Worker `/v1/hosts/*`: `local-mcp gateway-agent` が使用する bearer-token-protected API です。

reconnect 時は Durable Object generation を増やします。古い generation または古い process `instance_id` からの request/response は HTTP 409 で拒否します。tool は non-idempotent の可能性があるため、timeout した tool call を gateway が自動 retry することはありません。

## Configure and deploy

1. `wrangler.toml` に non-secret Access value を設定します。

   - `ACCESS_TEAM_DOMAIN`
   - `ACCESS_AUDIENCE`
   - `ACCESS_ALLOWED_EMAILS`

2. high-entropy host token を作成し、Worker secret として保存します。

       cd gateway
       npx wrangler secret put HOST_TOKEN

   `CLIENT_TOKEN` は local development 用の optional fallback です。production の ChatGPT traffic では verified `Cf-Access-Jwt-Assertion` を使用してください。

3. Worker と Durable Object exports configuration を deploy します。

       npm test
       npx wrangler deploy --dry-run
       npx wrangler deploy

4. custom domain を割り当て、Cloudflare Access self-hosted application で保護します。ChatGPT MCP connection 用に Managed OAuth を有効化します。Access 保護された custom hostname を public route とするため `workers_dev = false` を維持します。

5. host agent は Access service-token policy で通過させます。service-token client ID/secret は各 endpoint のみに保存してください。Worker はさらに `HOST_TOKEN` を要求するため、Access service token 自体は host protocol credential ではありません。

public MCP URL の例:

    https://<gateway-host>/mcp

## Start endpoint agents

各 project directory から local approval/session UI を開始します。

    local-mcp start mac-main

その terminal で `gateway_connect` を承認後、outbound agent を開始します。

    export LOCAL_MCP_GATEWAY_URL=https://<gateway-host>
    export LOCAL_MCP_GATEWAY_HOST_TOKEN='<worker HOST_TOKEN>'
    export LOCAL_MCP_GATEWAY_ACCESS_CLIENT_ID='<Access service-token ID>'
    export LOCAL_MCP_GATEWAY_ACCESS_CLIENT_SECRET='<Access service-token secret>'
    local-mcp gateway-agent --session-id mac-main

Windows endpoint では別の session ID を使用してください。native Windows transport/sandbox が実装されるまでは WSL2 内で両 command を実行します。

    local-mcp start windows-wsl2-main
    local-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2

`--platform auto` は macOS、通常 Linux、WSL2 を検出します。session ID は routing key であって credential ではありません。endpoint approval、Access policy、host token は引き続き必須です。

## Operational behavior

- Agent connect: generation を増やし、同じ `session_id` の以前の host を置き換えます。
- Agent poll: 90秒 lease を更新し、work を最大20秒待ちます。
- Tool dispatch: endpoint response を最大35秒待ちます。
- Disconnect / lease expiry: pending call は失敗し、replay しません。
- active call 中の Worker upgrade: caller には retryable gateway error を返しますが、gateway は endpoint operation を再実行しません。
- `session_list`: Registry lease を返します。最終的な online validation は per-session Durable Object 側でも実施します。

local Worker development では `.dev.vars.example` を `.dev.vars` にコピーします。`.dev.vars`、Worker secret、Access service-token secret、endpoint environment file は commit しないでください。
