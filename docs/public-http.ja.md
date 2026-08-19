# 公開 HTTP endpoint

[English](public-http.md)

Temote MCP は `/mcp` を Cloudflare Access で保護した on-demand Cloudflare Tunnel 経由で公開できます。

```text
MCP client
    | OAuth
    v
Cloudflare Access -- Cloudflare Tunnel -- 127.0.0.1:8791
                                               |
                                               v
                                        temote-mcp serve
```

## environment

設定ファイルは repository 外に置き、mode 0600 にします。

```sh
install -d -m 700 ~/.config/temote-mcp
cp .env.example ~/.config/temote-mcp/public.env
chmod 600 ~/.config/temote-mcp/public.env
```

必要な値:

- `TEMOTE_MCP_PUBLIC_URL`
- `TEMOTE_MCP_ACCESS_TEAM_DOMAIN`
- `TEMOTE_MCP_ACCESS_AUDIENCE`
- `TEMOTE_MCP_ACCESS_ALLOWED_EMAILS`
- `~/.config/temote-mcp/tunnel-token`（mode `0600`。変更する場合は `TUNNEL_TOKEN_FILE`）

`just env-check` は secret value を表示せず設定有無を確認します。

## 起動

`just up` で build、origin、Tunnel をまとめて起動できます。個別に起動する場合:

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
cloudflared tunnel run --token-file "${TUNNEL_TOKEN_FILE:-$HOME/.config/temote-mcp/tunnel-token}"
```

project ごとの local session は別 process として起動しておく必要があります。

## Cloudflare Access

`https://temotemcp.example.com/mcp` のような route では次の構成にします。

1. remotely managed Tunnel の hostname を `http://127.0.0.1:8791` へ向けます。
2. hostname 全体を self-hosted Cloudflare Access application で保護します。Managed OAuth discovery が host root の `/.well-known/` を使うため、application を `/mcp` だけに制限しません。
3. 利用対象 identity だけを Allow policy に入れます。公開 MCP route に Bypass は使いません。
4. 利用する MCP/OAuth client 向けに Managed OAuth を有効化します。dynamic client registration、token/grant lifetime、redirect URI、loopback option は client 要件に合わせます。
5. hostname を保護する self-hosted application の `AUD` を `TEMOTE_MCP_ACCESS_AUDIENCE` に設定します。

Cloudflare の `AI controls > MCP servers` に作る portal registration は、hostname を保護する self-hosted Access application とは別です。

Rust origin は転送された `Cf-Access-Jwt-Assertion` の signature、issuer、audience、expiry、subject、email allow list を検証します。

## probe

MCP client を接続する前に Access が origin より手前で応答することを確認できます。

```sh
curl -i -X POST https://temotemcp.example.com/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'

curl -i https://temotemcp.example.com/.well-known/oauth-authorization-server
curl -i https://temotemcp.example.com/.well-known/oauth-protected-resource
```

未認証の `/mcp` は Cloudflare の `WWW-Authenticate` 付き `401`、discovery endpoint は JSON metadata を返すのが正常です。`530` は通常 Tunnel または origin が停止しています。Cloudflare challenge のない Rust JSON `401` や discovery `404` は、Access application が想定 hostname/path を保護していない可能性があります。

origin が Access audience 不一致を報告した場合は、hostname を保護する self-hosted application の `AUD` を設定し直して `temote-mcp serve` を再起動します。

## 公開 tool の境界

公開 HTTP も local stdio と同じ session model を使いますが、`without_sandbox` は公開しません。ただし明示的に `--yolo` で起動した session の通常 command tool は unrestricted host permission で動きます。Cloudflare Access は authentication boundary であり、session mode の代替ではありません。
