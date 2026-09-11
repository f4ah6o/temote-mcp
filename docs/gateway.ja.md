# Multi-host Cloudflare gateway

[English](gateway.md)

任意機能の `gateway/` Worker は、複数の Temote host を1つの MCP endpoint の背後に federation します。macOS、Linux、Windows 11 上の WSL2 で、それぞれ1つの supervisor と1つの host-level gateway agent を動かし、MCP client は host と session を選択できます。マシンごとに MCP server entry を追加する必要はありません。

native Windows 実行は後続 milestone です。現時点の Windows 11 federation は WSL2 内で Temote を動かします。

## Architecture

- `GatewaySession` Durable Object を request/response queue として使います。host mode は `host:<host_id>`、legacy per-session mode は従来どおり `session_id` を key にします。
- `GatewayRegistry` は federated host lease と legacy per-session lease を別々に bounded state として保持します。
- Worker `/mcp` は MCP client を認証し、host-aware tool を公開して routing します。
- `/v1/hosts/*` は `temote-mcp gateway-agent` が outbound long poll で使う protocol です。
- session lifecycle、named-root 解決、sandbox、approval の最終 authority は各 host の local supervisor です。gateway には named root の absolute path を送信しません。

host reconnect ごとに generation を進めます。古い generation または古い process `instance_id` からの request/response は拒否します。routed operation には非 idempotent なものがあるため、timeout や disconnect 後の自動 replay は行いません。

## Host identity と認証

`host_id` は `mac-main`、`linux-main`、`win-main` のような安定した non-secret routing identity です。credential ではありません。

federated host mode では、Worker secret `HOST_TOKENS_JSON` に host ごとの bearer token map を保存します。例:

```json
{"mac-main":"<random-token-a>","linux-main":"<random-token-b>","win-main":"<random-token-c>"}
```

各 local host には自分専用の token だけを `TEMOTE_MCP_GATEWAY_HOST_TOKEN` で渡します。agent は `X-Temote-Host-Id` も送信し、Worker は `HOST_TOKENS_JSON` からその `host_id` に対応する credential を選びます。別 host の token を使って別の `host_id` を名乗る要求は拒否し、request body の `host_id` も header と一致しなければなりません。

`HOST_TOKEN` は一時的に残す legacy `gateway-agent --session-id` compatibility path 専用です。Cloudflare Access service-token credential は、どちらの Temote host token とも別 credential です。

## Deploy

1. `gateway/wrangler.toml` に non-secret の `ACCESS_TEAM_DOMAIN`、`ACCESS_AUDIENCE`、`ACCESS_ALLOWED_EMAILS` を設定します。
2. host ごとの token map を保存します。

```sh
cd gateway
npx wrangler secret put HOST_TOKENS_JSON
```

3. migration 中に旧 per-session agent も稼働させる場合は、既存の `HOST_TOKEN` Worker secret も保持します。
4. test と dry-run 後に deploy します。

```sh
npm test
npx wrangler deploy --dry-run --keep-vars
npx wrangler deploy --keep-vars
```

5. deploy 前に公開 target を1つ選びます。`gateway/wrangler.toml` は `workers_dev = false` を維持するため、target を指定しない deploy は version を upload しても公開されません。[Deployment target](#deployment-target) を参照してください。
6. 公開 hostname 全体を self-hosted Cloudflare Access application で保護し、利用する MCP client 向けに Managed OAuth を有効化します。
7. host agent は Access service-token policy で許可します。Access service token と Temote host bearer token は独立した credential です。

公開 MCP URL は `https://<gateway-host>/mcp` です。

## Deployment target

`workers_dev = false` では route または custom domain を指定しない deploy が Worker を公開せず、`No targets deployed` を表示することがあります。command が exit 0 でもこの出力は失敗として扱い、次のどちらか一方の target を明示します。

| Option | 使う条件 | target の設定 | Access | 確認 |
| --- | --- | --- | --- | --- |
| A. Custom domain | Worker に専用 hostname を割り当てる場合、または DNS record がまだ無い場合。 | hostname を Worker custom domain として宣言します。例: `gateway/wrangler.toml` の `routes = [{ pattern = "<gateway-host>", custom_domain = true }]`、または Cloudflare dashboard で作成します。 | hostname 全体を Cloudflare Access application で保護します。 | `npx wrangler deployments status --name temote-mcp-gateway` と `curl -sSf https://<gateway-host>/healthz`。 |
| B. Existing DNS + Worker route | 既存 DNS record を削除できない場合。 | deploy 時に exact pattern を渡します。例: `npx wrangler deploy --keep-vars --routes '<gateway-host>/*'`。 | hostname 全体を Cloudflare Access application で保護します。 | A と同じ。さらに Cloudflare dashboard で route pattern が `temote-mcp-gateway` を指すことを確認します。 |

どちらの方式でも次を守ります。

- `workers_dev = false` を維持し、`*.workers.dev` route を有効化しません。
- 既存 DNS record を削除しません。Worker route は一致する request で優先されますが、record は残るため direct origin へ戻せます。
- dashboard-managed の non-secret variable を保持する deploy では `--keep-vars` を使います。Worker secret は `wrangler secret` に残り、`wrangler.toml`、docs、issue tracker に値を書きません。
- Access service-token credential と gateway host token は別 credential として扱います。
- upload の成功を deploy の成功とみなしません。deploy 後は毎回 target を確認します。

### Deploy の確認（read-only）

```sh
npx wrangler deployments status --name temote-mcp-gateway
curl -sSf https://<gateway-host>/healthz
```

`/healthz` は `{"status":"ok","service":"temote-mcp-gateway"}` を返す必要があります。direct origin の応答や別 service の identity が返る場合、hostname はまだ意図した target を指していません。Access 経由の MCP 疎通は、この local check とは別に確認します。

### Rollback

rollback では追加した exact な Worker route または custom domain だけを外します。

1. Cloudflare dashboard で `<gateway-host>` の exact な route pattern または custom-domain binding を削除するか、`gateway/wrangler.toml` を戻して以前の target 構成を deploy します。
2. DNS record、Access application、Tunnel はそのまま残し、direct origin を維持します。
3. `curl -sSf https://<gateway-host>/healthz` と dashboard で hostname が Worker を指していないことを確認し、必要なら intended direct-origin 構成へ戻します。

gateway の rollback で無関係な route、DNS record、Access policy を削除・書き換えしないでください。

## 各 Temote host の設定

supervisor を動かすマシンごとに named root を設定します。gateway に広告するのは root 名だけで、physical path は host 内に残ります。

macOS 例:

```sh
export TEMOTE_MCP_ROOTS='{"src":"/Volumes/devstorage/Developer","work":"/Users/me/work"}'
export TEMOTE_MCP_GATEWAY_HOST_ID=mac-main
export TEMOTE_MCP_GATEWAY_URL=https://<gateway-host>
export TEMOTE_MCP_GATEWAY_HOST_TOKEN='<mac-main に割り当てた token>'
export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID='<Access service-token ID>'
export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET='<Access service-token secret>'

temote-mcp supervisor
temote-mcp gateway-agent --host-id mac-main
```

Linux も同じ構成で Linux path を使います。Windows 11 では WSL2 内で supervisor と agent を動かし、`/mnt/d/Developer` のような WSL path を named root にします。

```sh
export TEMOTE_MCP_ROOTS='{"src":"/mnt/d/Developer"}'
temote-mcp supervisor
temote-mcp gateway-agent --host-id win-main --platform wsl2
```

`--platform auto` は macOS、Linux、WSL2 を判別します。`TEMOTE_MCP_GATEWAY_HOST_ID` が設定されている場合、`temote-mcp doctor` は gateway readiness を stage 別に表示します。`local_config` の各項目（host ID、gateway URL origin、host token の存在、Access service-token の組）と `local_supervisor` の control protocol が個別の結果になります。doctor は root path や token 値を表示せず、remote endpoint / Access / host registration の stage は read-only remote 診断が実装されるまで未確認のままです。

## MCP workflow

gateway は host discovery と host-aware lifecycle routing を追加します。

```text
host_list()
host_info(host_id="linux-main")
session_list(host_id="linux-main")
session_start(host_id="linux-main", path="src/project-a", session_id="project-a")
session_info(host_id="linux-main", session_id="project-a")
```

`session_start` が受け付ける path は named-root-relative logical path だけです。host-side public supervisor が作成するのは常に通常の sandboxed session で、remote client から `--yolo` を要求することはできません。

同じ human-friendly `session_id` を複数 host で使えます。

```text
mac-main / srmj
linux-main / srmj
```

routing を確定させる場合は `host_id` を明示します。backward compatibility のため、`host_id` を省略した既存 `session_id` は、現在 discover 可能な owner がちょうど1つの場合だけ解決します。2 host が同じ ID を持つ場合や、leased host の問い合わせに失敗して ownership を安全に確定できない場合は、勝手に host を選ばず fail closed します。

`session_stop` と `session_restart` は、その host の public supervisor が所有する active managed session だけに作用します。別途 local CLI で起動した yolo session は local-only のままで、public session-bound tool は yolo target の unrestricted semantics を引き継がず拒否します。

## Lease と failure behavior

- poll ごとに90秒の host lease を更新し、最大20秒 work を待機します。
- gateway dispatch は endpoint response を最大35秒待ちます。
- `host_list` は registry entry だけで判断せず、対応する host Durable Object が active lease を返した host だけを表示します。
- reconnect は旧 generation を置き換え、古い agent instance を fence します。
- disconnect、lease expiry、timeout、Worker replacement のいずれでも、ambiguous な mutating tool call を自動 replay しません。
- unqualified な aggregate session discovery は、leased host の一部を問い合わせできない場合 fail closed します。
- sandbox と approval policy は実行 host で常に適用されます。

## Per-session agent からの migration

既存 command は migration 用に一時的に残します。

```sh
temote-mcp gateway-agent --session-id old-session
```

この mode は従来の `HOST_TOKEN` と、`session_id` を直接 key にした Durable Object を使います。新規構成では supervisor ごとに1つの `--host-id` agent を使ってください。

host mode は現行 supervisor control protocol を必要とします。この変更では control protocol を更新しているため、host-level gateway agent を起動する前に Temote supervisor を upgrade/restart してください。mixed version は lifecycle safety field を黙って落とさず、明示的に失敗します。

migration 中は legacy session agent と host-level agent を同時に利用できます。unqualified session routing は両方を確認し、collision があれば fail closed します。

## Development

local Worker 開発では `gateway/.dev.vars.example` を `gateway/.dev.vars` にコピーします。`.dev.vars`、Worker secret、Access service-token secret、host bearer token、endpoint environment file は commit しないでください。

routed tool schema と MCP protocol version は Rust 生成の contract snapshot に対して Rust/Node の両テストで照合します。`serverInfo.version` は Temote CLI CalVer ではなく、`GATEWAY_DEPLOYMENT` version-metadata binding が供給する Cloudflare deployment revision です。
