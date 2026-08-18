# local-mcp Cloudflare gateway

[English](README.md)

この Worker は ChatGPT 向けに1つの MCP エンドポイントを公開し、`session_id` に応じて Mac、Linux、Windows/WSL2 の各ホストへツール呼び出しを振り分けます。ホスト側から HTTPS long poll で接続するため、外向きにポートを開けたり、ホストごとに Tunnel を用意したりする必要はありません。

## 構成

`GatewaySession` は `session_id` ごとに1つ作られる Durable Object です。現在のホスト generation、request queue、処理待ちの response、host lease を持ちます。

`GatewayRegistry` は有効な session lease を集約し、`session_list` に使います。

Worker の `/mcp` は Cloudflare Access assertion を検証し、MCP の `initialize` と `tools/list` に応答します。`tools/call` は対象の `GatewaySession` へ転送します。

`/v1/hosts/*` は `local-mcp gateway-agent` 用の API で、bearer token で保護します。

ホストが再接続すると Durable Object の generation が増えます。古い generation や古い process `instance_id` から届いた request/response は HTTP 409 で拒否します。ツール呼び出しには非 idempotent な操作もあるため、timeout 後の自動 retry は行いません。

## 設定とデプロイ

1. `wrangler.toml` に、secret ではない Access の設定値を入れます。

   - `ACCESS_TEAM_DOMAIN`
   - `ACCESS_AUDIENCE`
   - `ACCESS_ALLOWED_EMAILS`

2. 十分に長いランダムな host token を作り、Worker secret として保存します。

       cd gateway
       npx wrangler secret put HOST_TOKEN

   `CLIENT_TOKEN` はローカル開発用の fallback です。本番の ChatGPT traffic では、検証済みの `Cf-Access-Jwt-Assertion` を使います。

3. テストと dry-run を通してからデプロイします。

       npm test
       npx wrangler deploy --dry-run
       npx wrangler deploy

4. custom domain を割り当て、Cloudflare Access の self-hosted application で保護します。ChatGPT の MCP 接続用に Managed OAuth を有効にしてください。公開経路を Access 配下の custom hostname に限定するため、`workers_dev = false` のまま使います。

5. host agent は Access の service-token policy で通します。service-token の client ID/secret は各 endpoint にだけ保存してください。Worker は別に `HOST_TOKEN` も確認するため、Access service token と host protocol の認証情報は別物です。

公開 MCP URL は次の形になります。

    https://<gateway-host>/mcp

## endpoint agent の起動

まず、対象プロジェクトのディレクトリでローカルセッションを起動します。

    local-mcp start mac-main

その端末で `gateway_connect` を承認してから、outbound agent を起動します。

    export LOCAL_MCP_GATEWAY_URL=https://<gateway-host>
    export LOCAL_MCP_GATEWAY_HOST_TOKEN='<worker HOST_TOKEN>'
    export LOCAL_MCP_GATEWAY_ACCESS_CLIENT_ID='<Access service-token ID>'
    export LOCAL_MCP_GATEWAY_ACCESS_CLIENT_SECRET='<Access service-token secret>'
    local-mcp gateway-agent --session-id mac-main

Windows 側は別の session ID にしてください。native Windows transport/sandbox が入るまでは、セッションと agent の両方を WSL2 内で起動します。

    local-mcp start windows-wsl2-main
    local-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2

`--platform auto` は macOS、通常の Linux、WSL2 を判別します。session ID は接続先を選ぶための routing key で、認証情報ではありません。endpoint 側の承認、Access policy、host token は別に必要です。

## 動作

ホストが接続すると generation が増え、同じ `session_id` にいた古いホストを置き換えます。

agent は poll のたびに90秒の lease を更新し、最大20秒 request を待ちます。ツール呼び出しを受けた gateway は endpoint の response を最大35秒待ちます。

接続が切れた場合や lease が切れた場合、処理待ちの call は失敗します。自動 replay はしません。実行中に Worker が更新された場合も、caller には retryable な gateway error を返しますが、endpoint の操作自体はやり直しません。

`session_list` は Registry にある lease を返します。実際にそのホストがオンラインかどうかは、各 session の Durable Object 側でも確認します。

ローカルで Worker を動かす場合は `.dev.vars.example` を `.dev.vars` にコピーします。`.dev.vars`、Worker secret、Access service-token secret、endpoint の環境ファイルは commit しないでください。
