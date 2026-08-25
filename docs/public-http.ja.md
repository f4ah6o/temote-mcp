# Remote connection profile

[English](public-http.md)

Temote MCP は3つの production connection profile を提供します。Cloudflare / Tailscale は general-purpose な public HTTPS endpoint、OpenAI Secure MCP Tunnel は supported OpenAI product 向けの outbound-only private connection です。

| Profile | Connection | Authentication / trust boundary |
| --- | --- | --- |
| `cloudflare` | Cloudflare Tunnel public HTTPS | Cloudflare Access Managed OAuth |
| `tailscale` | Tailscale Funnel public HTTPS | Temote local OAuth |
| `openai` | OpenAI Secure MCP Tunnel | OpenAI tunnel connection + Temote local sandbox/approval |

`--profile` を省略した場合は既存互換のため `cloudflare` として動作します。3 profile とも同じ provider-neutral MCP core に到達します。remote access に `without_sandbox` は出ず、managed session の named root、sandbox、runtime approval の境界も profile によって変わりません。

## Cloudflare profile

```text
MCP client
    | Managed OAuth
    v
Cloudflare Access -- Cloudflare Tunnel -- 127.0.0.1:8791
                                               |
                                               v
                               temote-mcp up --profile cloudflare
```

Cloudflare の deployment 設定は repository 外に置き、mode `0600` にします。

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

Cloudflare profile は `~/.config/temote-mcp/public.env`（または `TEMOTE_MCP_ENV_FILE`）を読み込みます。origin 側でも、転送された `Cf-Access-Jwt-Assertion` の signature、issuer、audience、expiry、subject、設定済み email allow list を検証する既存 defense-in-depth を維持します。

lifecycle supervisor を先に起動し、origin と Tunnel は別 process として起動します。

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor
# 別 terminal/service
temote-mcp up --profile cloudflare
```

`--profile` なしの `temote-mcp up` も同じ動作です。`up/serve` は local supervisor control socket を必須とし、session runtime を所有しません。`temote-mcp down` が停止するのは HTTP origin と Temote が起動した Tunnel child だけで、lifecycle supervisor と session は生存します。

`cloudflared` を Temote から起動せず origin だけ実行する場合:

```sh
set -a
. ~/.config/temote-mcp/public.env
set +a
temote-mcp serve --profile cloudflare
```

`https://temotemcp.example.com/mcp` のような route では次の構成にします。

1. remotely managed Tunnel の hostname を `http://127.0.0.1:8791` へ向けます。
2. hostname 全体を self-hosted Cloudflare Access application で保護します。Managed OAuth discovery が host root の `/.well-known/` を使うため、Access を `/mcp` だけに限定しません。
3. 利用対象 identity だけを Allow policy に入れ、公開 MCP route に Bypass は使いません。
4. 対象 client 向けに Managed OAuth を有効化します。
5. self-hosted application の `AUD` を `TEMOTE_MCP_ACCESS_AUDIENCE` に設定します。

Cloudflare の `AI controls > MCP servers` に作る portal registration は、hostname を保護する self-hosted Access application とは別です。

## Tailscale profile

```text
MCP client
    | Authorization Code + PKCE S256
    v
Temote local OAuth -- Tailscale Funnel -- 127.0.0.1:8791
                                               |
                                               v
                                temote-mcp up --profile tailscale
```

Tailscale profile には、Funnel を利用可能な接続済み Tailscale CLI/node が必要です。Cloudflare account、Tunnel token、Access application、Access audience、email allow list は不要です。

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor
# 別 terminal/service
temote-mcp doctor --profile tailscale
temote-mcp up --profile tailscale
```

public URL を明示しない場合、Temote は `tailscale status --json` の `Self.DNSName` から canonical `*.ts.net` hostname を導出し、Funnel が利用できる HTTPS port を `443` → `8443` → `10000` の順で自動選択します。Temote 管理の Funnel は local `127.0.0.1` origin にのみ proxy します。既存 Funnel 設定は上書きせず、3 port がすべて使用中なら fail-closed します。

Tailscale daemon 自体は停止しません。`temote-mcp down` が停止するのは Temote の HTTP origin と、その process が直接起動した `tailscale funnel` child だけです。lifecycle supervisor と session は停止しません。

`temote-mcp serve --profile tailscale --public-url <https-origin>` は Funnel を起動せず local OAuth/MCP origin のみを実行します。ingress を別に管理する場合に利用できます。`temote-mcp up --profile tailscale` で public URL を明示する場合は、現在の node の `*.ts.net` hostname と Funnel 対応 HTTPS port (`443` / `8443` / `10000`) を使う必要があります。

Tailscale profile は Cloudflare 用 `public.env` を読みません。shell の `TEMOTE_MCP_PUBLIC_URL` は Tailscale hostname を導出できない場合の fallback で、`serve` で明示的に上書きする場合は `--public-url` を使います。

### Temote local OAuth

local authorization server は以下を公開します。

```text
/.well-known/oauth-protected-resource
/.well-known/oauth-authorization-server
/register
/authorize
/token
/mcp
```

Authorization Code flow と mandatory PKCE `S256` を使用します。authorization code は短時間のみ有効・single-use で、exact `client_id`、redirect URI、MCP resource に binding されます。access token は exact `/mcp` resource に binding された短寿命 opaque bearer token です。code/token value は通常ログや approval summary に出しません。

client discovery は現行の Client ID Metadata Documents に対応し、client compatibility のため Dynamic Client Registration `/register` も維持します。metadata document の取得先は HTTPS port 443 の public DNS のみに限定し、redirect は追跡せず、private / loopback / special-use address と過大 response を拒否します。

初回 authorization は `temote-mcp session console` で owner が承認します。`serve/up` は owner-only supervisor control socket 経由で request を転送し、HTTP 上に approval interface は公開しません。表示するのは client、redirect URI、resource、scope です。この OAuth approval と、その後の host/network-sensitive tool の runtime approval は別の security decision です。authentication に成功しても yolo session は作りません。

registration、pending authorization code、access token は bounded な process-local state です。Temote を再起動すると local OAuth state は無効になります。password database、email database、persistent bearer-token file は不要です。

## OpenAI Secure MCP Tunnel profile

`openai` profile は、OpenAI Secure MCP Tunnel を通じて private/local MCP server に到達できる supported OpenAI product 向けです。public Internet endpoint は作成せず、`TEMOTE_MCP_PUBLIC_URL`、Cloudflare、Tailscale を要求しません。

Temote から OpenAI Tunnel Management API を呼んで tunnel record を作成できます。対話利用では `OPENAI_ADMIN_KEY` が無ければ `openai setup` が controlling terminal から echo 無効で Admin API key を読みます。

```sh
temote-mcp openai setup --workspace-id '<CHATGPT_WORKSPACE_ID>'
```

`openai setup` は既定で `POST https://api.openai.com/v1/tunnels` を実行し、少なくとも1つの `--workspace-id` または `--organization-id` を要求します。返された `CONTROL_PLANE_TUNNEL_ID` だけを `~/.config/temote-mcp/openai.env` に private permission で保存し、API key は保存しません。既存 tunnel ID は `--force` を明示しない限り置換しません。公式の `CONTROL_PLANE_BASE_URL` override も、credential/path を含まない HTTPS origin に限って利用できます。非対話 setup 向けには `OPENAI_ADMIN_KEY` も引き続き利用できます。

Runtime API key は別 credential で、この command からは作成しません。**Tunnels Read + Use** を持つ Restricted Runtime API key を作成します。`temote-mcp up --profile openai` は `CONTROL_PLANE_API_KEY` または公式 fallback の `OPENAI_API_KEY` があれば利用し、どちらも無ければ controlling terminal から echo 無効で Runtime API key を読みます。prompt した値は argv、shell environment、Temote config には書かず、Temote-owned `tunnel-client` child environment にだけ注入し、spawn 後に元 buffer を zeroize します。`OPENAI_ADMIN_KEY` は runtime child から明示的に除外します。

Temote は公式 `openai/tunnel-client` と統合します。runtime には以下が必要です。

- `PATH` 上の `tunnel-client`、または binary を指す `TUNNEL_CLIENT_BIN`
- environment または bootstrap state `openai.env` に保存された `CONTROL_PLANE_TUNNEL_ID`
- Restricted Runtime API key。対話 `up` では hidden prompt、非対話運用では environment から渡せます

`doctor --profile openai` は意図的に non-interactive です。control-plane access を検証する場合は `CONTROL_PLANE_API_KEY` または公式 fallback の `OPENAI_API_KEY` を environment から受け取ります。通常の対話 start では不要です。

```sh
temote-mcp up --profile openai
```

`temote-mcp up --profile openai` は local MCP origin を loopback にだけ bind し、概念上次の Temote-owned child を起動します。

```text
tunnel-client run \
  --control-plane.tunnel-id <configured tunnel> \
  --mcp.server-url http://127.0.0.1:8791/mcp
```

local port は `--addr` に従い、non-loopback bind は拒否します。`temote-mcp down` が停止するのは Temote の HTTP origin と直接起動した `tunnel-client` child だけで、lifecycle supervisor と session は停止しません。public listener、public OAuth server、Cloudflare Tunnel、Tailscale Funnel は作成しません。

tunnel 接続成功を yolo の根拠にはしません。remote tool call は同じ managed-session / named-root / sandbox / host・network-sensitive approval 境界へ入ります。OpenAI tunnel から提供されない identity claim を Temote 側で推測して生成しません。

公式資料: [OpenAI tunnel-client](https://github.com/openai/tunnel-client)、[ChatGPT developer mode / MCP connectors](https://help.openai.com/en/articles/12584461-developer-mode-and-full-mcp-connectors-in-chatgpt)。

## 診断

profile-aware な診断は明示的に実行できます。

```sh
temote-mcp doctor --profile cloudflare
temote-mcp doctor --profile tailscale
temote-mcp doctor --profile openai
```

Cloudflare profile は `cloudflared`、private Tunnel token file、Access configuration を検査します。Cloudflare API から Tunnel status も取得する場合は `--cloudflare` を追加します。追加の diagnostic environment variable は [development diagnostics](development.md) に記載しています。

Tailscale profile は CLI/node connection、canonical `*.ts.net` endpoint、HTTPS port `443` / `8443` / `10000` の既存 Funnel ownership、安全に利用できる次の port、local OAuth state を検査します。Cloudflare credential は検査しません。

OpenAI profile は公式 `tunnel-client`、tunnel ID/runtime key 設定、credential がある場合の control-plane access、loopback-only local bind policy を検査します。Cloudflare / Tailscale の設定は要求しません。

bare `temote-mcp doctor` は従来の local behavior を維持し、Cloudflare Tunnel 設定が存在する場合または明示指定された場合だけ Cloudflare local check を行います。

## MCP protocol compatibility

公開 endpoint は MCP `2026-07-28` と既存の 2025 系 handshake の両方に対応します。modern request は `server/discover`、request ごとの `_meta`、`MCP-Protocol-Version` / `Mcp-Method` / `Mcp-Name` HTTP header を使います。legacy client は従来どおり `initialize` を使い、modern request では `Mcp-Session-Id` を作りません。

Tailscale profile の未認証 `/mcp` は `401` と Bearer `WWW-Authenticate` challenge を返し、protected-resource metadata URL を含めます。Cloudflare profile では引き続き Cloudflare Access が外部 Managed OAuth boundary であり、Rust origin も invalid / missing Access assertion を拒否します。

## Remote tool / managed session の境界

`TEMOTE_MCP_ROOTS` が設定されている場合、認証済み HTTP client は `session_start` / `session_stop` を利用できます。`session_start` は logical named-root-relative path のみ受け付け、yolo option はありません。absolute path、unknown root、traversal、symlink escape、roots 未設定時の fallback は拒否します。`session_stop` は現在の `serve` process が所有する session に限定されます。

remote profile に `without_sandbox` は出ません。通常 session は filesystem containment と network-disabled sandbox を維持し、host/network-sensitive operation は引き続き local approval が必要です。公開 HTTP authentication は identity boundary であり、Temote の session / sandbox / approval boundary の代替ではありません。
