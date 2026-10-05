# Cloudflare 上の Temote Fabric

[English](gateway.md)

任意機能の `fabric/` Worker は、複数の Temote host を1つの MCP endpoint の背後に federation します。macOS、Linux、Windows 11 上の WSL2 で、それぞれ1つの supervisor と1つの host-level gateway agent を動かし、MCP client は host と session を選択できます。マシンごとに MCP server entry を追加する必要はありません。

native Windows 実行は後続 milestone です。現時点の Windows 11 federation は WSL2 内で Temote を動かします。

## Architecture

- `GatewaySession` Durable Object を request/response queue として使います。host mode は `host:<host_id>`、legacy per-session mode は従来どおり `session_id` を key にします。
- `GatewayRegistry` は federated host lease と legacy per-session lease を別々に bounded state として保持します。
- Worker `/mcp` は MCP client を認証し、host-aware tool を公開して routing します。
- `/v1/hosts/*` は `temote-mcp gateway-agent` が outbound long poll で使う protocol です。
- session lifecycle、named-root 解決、sandbox、approval の最終 authority は各 host の local supervisor です。gateway には named root の absolute path を送信しません。
- delegation observation は owner 専用の local journal に先に記録します。host agent は認証済み host channel から送信対象の record を D1 へ複製します。D1 の内容は sanitized replica であり、execution の authority にはなりません。
- D1 への ingest が commit されると Queue が Memory Worker を起動します。D1 は未処理 work を保持し、定期 sweep が Queue 送信漏れと projection の遅れを回復します。
- knowledge は根拠を持つ derived projection です。execution を認可したり起動したりしません。

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

現在の deploy は pinned `cf` CLI と `cloudflare.config.ts` を使います。
ignored の `TEMOTE_DEPLOYMENT_CONFIG` profile で Worker、D1、domain と
non-secret な Access policy を指定します。既存 Gateway は
[段階的な naming migration](naming-migration.md) に従い、namespace ID、
D1 ID、source の credential authority を保持します。migration 前に
D1 Time Travel bookmark を取得し、適用後に外部キーの整合性を確認します。

```sh
# repository root
just generate-tools
just check-generated
(cd fabric && npm test)
# Fabric source directory; reviewed non-secret profile を指定
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf build
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf deploy --prebuilt --dry-run
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf deploy --prebuilt
```

既存 secret は名前で継承します。新規 secret は認可済みの runtime injection
を使い、profile や command に値をコピーしません。Events と memory は
依存機能が設定されるまで無効です。upload や dry-run の成功だけでは、
domain、Access 認証、Host/session の稼働は確認できません。

### Wrangler compatibility deployment

以下は既存 operator 向けの Wrangler 互換手順です。flag は Wrangler に
適用します。package の deployment script は現在 `cf` を呼び出します。

Temote Fabric は現在 `fabric/` に実装されています。repository root にある `just` command は repository root で実行し、npm と Wrangler は pinned package、lockfile、`wrangler.toml` のある `fabric/` で実行します。別々の code block の間で working directory は引き継がれません。

1. Node.js 22 以降を使い、deploy tooling を lockfile からインストールします。

```sh
(cd fabric && npm ci)
```

Rust の定義を変更した場合は repository root で生成・検証します。

```sh
just generate-tools
just check-generated
```

Worker は生成された `fabric/contract/routed-tool-metadata.json` を直接読みます。

Observation 用に `OBSERVATION_OWNER_ID` と D1 `OBSERVATION_DB` を設定し、sentinel database ID を実際の ID に置き換えます。
未適用 migration を確認してから、`fabric/` の pinned Wrangler で適用します。
`0003_memory_worker.sql` は既存の knowledge table を再構築し、item、support、supersession の行をコピーします。
対象 database の内容に照らして、この migration を事前に確認してください。

```sh
(cd fabric && npx wrangler d1 migrations list temote-observation --remote)
(cd fabric && npx wrangler d1 migrations apply temote-observation --remote)
```

既存 Worker を更新するときは Durable Object の class 名と binding を維持します。
dry-run の成功は remote secret、認証、公開 endpoint の成功を意味しません。

2. 公開 target を1つ選び、既存の所有先を確認します。[Deployment target](#deployment-target) を参照してください。`workers_dev = false` を維持します。
3. **公開する前に** hostname 全体を self-hosted Cloudflare Access application で保護します。人間の MCP client 向けに Managed OAuth、host agent 向けに Service Auth policy を設定します。
4. deployment config に non-secret の `ACCESS_TEAM_DOMAIN`、`ACCESS_AUDIENCE`、`ACCESS_ALLOWED_EMAILS` と前述の Observation 設定を用意します。
5. host ごとの token map を対話入力で保存します。旧 per-session agent が必要とする場合だけ、既存の `HOST_TOKEN` Worker secret も保持します。Access service token と Temote host bearer token は独立した credential です。

```sh
(cd fabric && npx wrangler secret put HOST_TOKENS_JSON)
```

6. metadata、test、dry-run を検証し、remote 設定と secret の存在を確認してから選択した target へ deploy します。

```sh
# repository root
just generate-tools
just check-generated

# fabric/
(cd fabric && npm test)
(cd fabric && npx wrangler deploy --dry-run --keep-vars)
(cd fabric && npx wrangler deploy --keep-vars)
```

7. 下記の health と認証済み MCP 疎通を確認します。target を指定しない deploy は version を upload しても公開されません。

公開 MCP URL は `https://<gateway-host>/mcp` です。

D1 schema は observation、memory run、checkpoint、outbox、knowledge の table を使います。
適用前に未適用 migration を確認してください。
`0003_memory_worker.sql` は既存の knowledge table を再構築し、既存の row と provenance をコピーします。
対象 database に適用する前に内容を確認してください。
memory extraction は既定で無効です（`MEMORY_ENABLED = "false"`）。
OpenAI-compatible extractor を有効にする場合は `MEMORY_ENABLED = "true"`、`MEMORY_EXTRACTOR = "openai_compatible"`、full URL の `MEMORY_ENDPOINT`、`MEMORY_MODEL`、`MEMORY_TIMEOUT_MS`、`MEMORY_INPUT_BUDGET_BYTES`、`MEMORY_OUTPUT_BUDGET_BYTES`、`MEMORY_MAX_ATTEMPTS`、`MEMORY_BATCH_SIZE`、`MEMORY_PROJECTION_GENERATION` を Worker config に設定します。
`MEMORY_API_KEY` は Worker secret として保存し、値を `wrangler.toml`、`.dev.vars.example`、評価 artifact に書きません。
抽出方式を変えるときは `MEMORY_PROJECTION_GENERATION` を単調増加させ、旧 worker が有効な projection を置き換えないようにします。

必要な場合は `MEMORY_REASONING_EFFORT` に `low`、`medium`、`high`、`minimal`、`none`、`max`、`xhigh` のいずれかを指定できます。
未設定なら adapter は request に `reasoning_effort` を含めず、provider の既定値を使います。
ローカルの許可リスト外の値は、requestを送信せず `provider_configuration_invalid` として報告されます。
許可リスト内でも選択したproviderが対応しない値は、HTTP 400 と `provider_rejected` として報告されます。
OpenCode Go は `glm-5.3-flash` を提供しており、GLM API の仕様では GLM-5.3-Flash の thinking は無効化できず、既定 effort は `max`、`low` は指定可能です。
このモデルで bounded live extraction を行う場合は `low` を明示します（[OpenCode Go の model と endpoint](https://opencode.ai/docs/go/)、[GLM Chat Completion の parameter](https://docs.z.ai/api-reference/llm/chat-completion)）。
producer version は adapter version、指定された effort、envelope 上限を追跡します。
adapter の意味や reasoning effort を変える場合は `MEMORY_PROJECTION_GENERATION` を単調増加させ、derived projection を再構築します。

`MEMORY_OUTPUT_BUDGET_BYTES` は抽出する JSON content の上限です。
これとは別に、HTTP response の JSON envelope は `min(80 KiB, 2 × MEMORY_OUTPUT_BUDGET_BYTES + 16 KiB)` 以内に制限します。
既定の content budget 8 KiB では envelope は最大32 KiB、最大の content budget 32 KiB では最大80 KiBです。
adapter は response の `content` だけを読み取ります。
provider の `reasoning_content` と `usage` は無視し、記録、保存、再利用しません。
envelope 超過は `provider_envelope_too_large`、provider が `finish_reason: "length"` を返した completion は `provider_incomplete_response` として `context_status` に報告され、どちらも checkpoint を進めません。
provider または response budget を修正した後、`MEMORY_PROJECTION_GENERATION` を増加させると retained observation を新しい projection として再処理します。

```sh
(cd fabric && npx wrangler secret put MEMORY_API_KEY)
```

`wrangler secret put` は直ちに Worker deployment を作成します。
target と Access policy を確認し、意図した Worker の設定作業として実行してください。
[Wrangler の secret 管理](https://developers.cloudflare.com/workers/configuration/secrets/) を参照してください。
抽出を止めるときは `MEMORY_ENABLED` を `false` にします。
有効にするときは `true` にして、確認済みの Worker config を deploy します。
`temote-memory` Queue と5分ごとの scheduled sweep は `fabric/wrangler.toml` に定義されています。
deploy 時も両方の binding を維持してください。
Queue は at-least-once の起動通知です。
D1 outbox は未送信または古くなった work を60秒後に再送対象にし、次の5分ごとの scheduled sweep が Queue に再投入します。
provider run は `MEMORY_MAX_ATTEMPTS` で再試行を打ち切り、上限後は `context_status` に bounded error code と `failed` を報告します。
sweep はこの上限を迂回しません。
provider または入力の失敗原因を修正した後、retry するには `MEMORY_PROJECTION_GENERATION` を単調増加させ、確認済みの config を deploy します。
新しい producer version は retained observation を先頭から再処理します。
再構築が追いつくまで、以前の active projection は読み取り可能です。
provider cost を伴う明示的な rebuild として扱ってください。
同期と Worker の freshness は `context_status` で確認します。
live deployment では `fixture` extractor を有効にしないでください。

Memory Worker は1 invocation につき repository group を最大1つ処理します。
保守的に見積もった worst-case は602 D1 statement です。
内訳は setup と claim が最大14、projection lookup が最大48（生成 summary を含むknowledge item最大24件について、active-row と support-detail を各1回）、supersession provenance read が最大24、commit batch が最大512、commit 後と pending outbox 回復の処理が計4（commit 後のreadが1、outbox回復が3）です。
support-detail の read は以前の count read を置き換えるもので、合計 statement 数は増やしません。
[Cloudflare D1 の公式上限](https://developers.cloudflare.com/d1/platform/limits/) では Workers Paid の1 invocation あたり上限は1,000 query なので、この実装上の statement 数はその範囲に収まります。
Free の上限は50 query で worst-case batch を処理できないため、Free の worst-case qualification はしていません。
この statement 数の qualification は、特定の account、plan、remote Worker の確認を示すものではありません。

projection の準備に512 statement を超える場合、Worker は `projection_too_large` を返し、knowledge を commit せず checkpoint も進めません。
再試行は `MEMORY_MAX_ATTEMPTS` で打ち切ります。
復旧するには `MEMORY_BATCH_SIZE` を下げて入力を commit 上限内に収め、`MEMORY_PROJECTION_GENERATION` を増加させて確認済み config を deploy し、retained observation から rebuild します。
新しい projection の publish までは、以前の active projection を読み取れます。

repository-scoped context は、観測済みの D1 record から取得できます。
live Worker が offline でも利用できます。
Cloudflare 上でこの経路が動くことは、ローカルの dry-run だけでは確認できません。

## Deployment target

`workers_dev = false` では route または custom domain を指定しない deploy が Worker を公開せず、`No targets deployed` を表示することがあります。command が exit 0 でもこの出力は失敗として扱い、次のどちらか一方の target を明示します。

Wrangler を実行する前に、意図した target を `fabric/` から repository-local preflight で確認します。

```sh
cd fabric
npm run deploy:preflight -- --hostname gateway.example.com --route 'gateway.example.com/*'
```

preflight は `target_missing`、`target_mismatch`、`remote_unknown` を区別して返します。`remote_unknown` は Cloudflare credential を使わないこの local check が、deploy 済み route や custom domain の存在を成功と偽装しないための状態です。non-zero 結果は target を修正し、下記の deploy 後 read-only verification が成功するまで停止として扱います。

| Option | 使う条件 | target の設定 | Access | 確認 |
| --- | --- | --- | --- | --- |
| A. Custom domain | Worker に専用 hostname を割り当てる場合、または DNS record がまだ無い場合。 | hostname を Worker custom domain として宣言します。例: `fabric/wrangler.toml` の `routes = [{ pattern = "<gateway-host>", custom_domain = true }]`、または Cloudflare dashboard で作成します。 | hostname 全体を Cloudflare Access application で保護します。 | Wrangler status は `fabric/` から実行し、下記の Access 認証付き `/healthz` を確認します。 |
| B. Existing DNS + Worker route | 既存 DNS record を削除できない場合。 | deploy 時に exact pattern を渡します。例: `npx wrangler deploy --keep-vars --routes '<gateway-host>/*'`。 | hostname 全体を Cloudflare Access application で保護します。 | A と同じ。さらに Cloudflare dashboard で route pattern が `temote-mcp-gateway` を指すことを確認します。 |

どちらの方式でも次を守ります。

- `workers_dev = false` を維持し、`*.workers.dev` route を有効化しません。
- 既存 DNS record を削除しません。Worker route は一致する request で優先されますが、record は残るため direct origin へ戻せます。
- dashboard-managed の non-secret variable を保持する deploy では `--keep-vars` を使います。Worker secret は `wrangler secret` に残り、`wrangler.toml`、docs、issue tracker に値を書きません。
- Access service-token credential と gateway host token は別 credential として扱います。
- upload の成功を deploy の成功とみなしません。deploy 後は毎回 target を確認します。

### Deploy の確認（read-only）

Wrangler command は `fabric/` から実行します。hostname 全体を Access で保護しているため、health check には Service Auth policy で許可された service-token credential を渡します。credential 値は保護された environment または secret store に置き、command や log に直接書かないでください。

```sh
(cd fabric && npx wrangler deployments status --name temote-mcp-gateway)
curl --silent --show-error --fail \
  --header "CF-Access-Client-Id: ${TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID:?Access service-token client ID を設定してください}" \
  --header "CF-Access-Client-Secret: ${TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET:?Access service-token client secret を設定してください}" \
  "https://<gateway-host>/healthz"
```

`/healthz` は Temote gateway の identity と `readiness=ready` を返す必要があります（現在の形式は `{"status":"ok","service":"temote-fabric","readiness":"ready","identity":"temote-fabric","compatibilityIdentity":"temote-mcp-gateway","contractFingerprint":"<sha256>"}`）。Worker 内の `/healthz` handler は client token を要求しませんが、Cloudflare Access は edge で hostname を保護します。direct origin の応答や別 service の identity が返る場合、hostname はまだ意図した target を指していません。`contractFingerprint` は public tool contract の SHA-256 digest で、deploy した source revision の `fabric/contract/public-tools.fingerprint` と、local/connected server の `session_info` が返す `server_contract_fingerprint` に一致する必要があります。

MCP `tools/list` や他の `/mcp` request は、`ACCESS_ALLOWED_EMAILS` に含まれる user として Access Managed OAuth で認証した MCP client から確認します。Worker は Access JWT の signature、audience、issuer、expiry、subject、allowlist 内 email を検証します。Access service token は host agent の Service Auth と `/healthz` smoke 用です。service-token JWT には user email がなく `sub` も空のため、`/mcp` の user identity check は通りません。local/test 用の `CLIENT_TOKEN` を production Worker に設定しないでください。

### Rollback

rollback では追加した exact な Worker route または custom domain だけを外します。

1. Cloudflare dashboard で `<gateway-host>` の exact な route pattern または custom-domain binding を削除するか、`fabric/wrangler.toml` を戻して以前の target 構成を deploy します。
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

既存の host-level `gateway-agent --host-id` は、認証済み channel から observation batch を自動送信します。
1 batch は最大32件または512 KiBで、5秒ごとに確認し、同期 request の timeout は8秒です。
local journal への記録が先行するため、Fabric の timeout や認証失敗で delegated task が失敗したり、backend operation が再実行されたりしません。
agent は通常の `task_get` polling が完了を観測して作成した terminal observation を含む retained journal を確認し、host、Fabric endpoint、session ごとの cursor を owner 専用領域に保存します。
呼び出し側の polling が止まった後に backend を独自に確認するわけではありません。
`committed_through_revision` は D1 への commit が確認された最大 source revision です。
`acked_through_revision` は D1 が連続して受領したと確認できる source revision です。
既知の gap を越えて後続の retained record を同期できますが、gap は保持され、source は `complete=false` になります。
source revision cursor と cloud sequence は別の番号体系です。

同期診断は stable error code と timestamp を含む owner 専用 status file に保存されます。
retry 間隔は2秒から最大5分まで段階的に延び、agent 再起動後も再開します。
同期 cursor、gap、source の partial 状態は `context_status({repository: ...})` で確認できます。
手動同期 command はありません。
cursor を reset する CLI もありません。
operator が承認した復旧で古い journal record を戻した場合は、対象 host の `gateway-agent` だけを停止し、`cursor` に `gateway-observation-sync/<host-id>/<sha256-normalized-gateway-url>/` 内の対象 session の `ack-<session-id>.json` のみを設定して、owner 専用の一意な backup 名へ移動します。

```sh
mv -- "$cursor" "${cursor}.backup-${unique_timestamp}"
```

その後、同じ agent を再起動すると retained record を再走査します。
observation journal、`status.json`、他の cursor は変更しません。
D1 の完全一致する再送は重複しませんが、再走査だけで既報の source gap は消えず、complete の証明にもなりません。
結果は `context_status` で確認します。

repository-wide context を利用できるのは、対応する Git forge の unambiguous な `remote.origin.url` を解決できる workspace です。
GitHub、GitLab.com、Bitbucket.org の remote を `github:owner/repository` のような path-free key に正規化します。
checkout path や directory 名は使いません。
remote を安全に解決できない場合、その observation は session scope にとどまり、repository-wide current knowledge には使いません。

instruction と error の preview は既定で cloud sync から除外します。
`TEMOTE_MCP_OBSERVATION_SYNC_PREVIEW=1` は bounded text preview の送信を opt-in します。
preview に秘密が含まれない保証はありません。
長さを制限しても secret は除去されないため、既存の content-sharing policy が許す場合だけ設定します。
対象 host / Fabric endpoint の未 ACK batch がなくなるまで、この設定を変更しないでください。
D1 commit 後に応答を失うと、再起動後に host は durable cursor から同じ observation を再送します。
preview 設定が変わってpayloadが異なる場合、D1 は既存 observation を上書きせず `409 conflicting_replay` を返します。
preview opt-out の再送には preview は含まれません。
再送を成功扱いにするために payload digest の検証を弱めないでください。
conflict が起きた場合、当初の preview policy に従う送信が引き続き認可されているときだけ、その policy に戻して ACK が進むまで同期します。
認可されていない場合は対象 host agent を停止し、source を診断状態のままにしてください。
設定を off にしても、すでに D1 へ commit された preview は取り消せません。

`--platform auto` は macOS、Linux、WSL2 を判別します。`TEMOTE_MCP_GATEWAY_HOST_ID` が設定されている場合、`temote-mcp doctor` は gateway readiness を stage 別に表示します。`local_config` の各項目（host ID、gateway URL origin、host token の存在、Access service-token の組）と `local_supervisor` の control protocol が個別の結果になります。network-enabled build では read-only の `/healthz` identity check と認証付き `/v1/hosts/status` probe も実行し、remote endpoint、Access 認証、この host の active lease を分類します。doctor は `session_availability` も local supervisor の read-only な session inventory から `listed_sessions`/`active_sessions` の件数として報告します。これは supervisor control protocol を再利用し、MCP tool を dispatch せず、session や lease を変更しません。live（`active`/`starting`）な session が 1 件もないと確定した inventory は `ready` ではなく `failed` とし、inventory を列挙できない場合は `unavailable` とします。local の host-level `gateway-agent` generation が記録されている場合、doctor は認証済み gateway の `generation` と比較し、remote の generation が新しいときは `generation_replaced` として報告するため、置き換えられた古い local agent を healthy と誤認しません。host-level `gateway-agent` は bounded で non-secret な `session_availability`（`ready`、`session_unavailable`、`unavailable`）を poll ごとに報告し、認証付き `/v1/hosts/status` は最新の報告値を返します。未報告の値は `ready` として扱わず `not_checked` のままにします。この remote 値は supervisor inventory から read-only で導出され、session ID、path、credential を含みません。root path や token 値は表示しません。

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

### Repository context と memory

Fabric は既存の `context_resolve` と `context_status` を拡張します。
repository を指定すれば `session_id` を省略でき、すべての execution host が offline でも認証済み D1 replica から取得できます。

```text
context_resolve({repository: "github:owner/repository", query: "report", budget_bytes: 16384})
context_status({repository: "github:owner/repository"})
```

owner scope は認証済み Worker config から決まり、caller は別 owner を指定できません。
repository key は安定した forge identity から作り、local directory 名は使いません。
session request は記録済みの owner、host、repository の対応を検証します。
session の cloud mapping がない場合は既存の host fallback を使います。
認可または所有権の検証に失敗した request は別 host へ fallback しません。

`context_resolve` は replicated task observation、support reference 付きの derived knowledge、関連 task、bounded freshness metadata を返します。
observation は `replicated_observed`、knowledge は `derived` と示し、replica を live state として報告しません。
`context_status` は source cursor と gap、observation freshness、Memory Worker の readiness と lag を報告します。
partial または stale な結果は明示されます。
response は deterministic で上限があり、raw observation body を含みません。
`MEMORY_ENABLED = "false"` のままでも observation に基づく repository context は利用でき、memory は disabled と報告されます。
その他の host-routed tool と local session-bound tool は既存の `session_id` 要件を維持します。

#### Knowledge の昇格と競合

Worker は各 support reference が cloud sync 対象の observation に存在することを確認し、引用文が元の本文にそのまま含まれることを検証します。
extractor は status や scope を決められず、`verification_path` は `null` でなければなりません。
confidence や observation の kind だけでは検証済みになりません。
直接引用で根拠を持つ主張は通常 `supported` になり、確認済み user instruction の一部だけが `current` に昇格します。

- 制約を repository-wide の `current` にするには、user instruction に明示的な repository policy marker と直接の制約文があり、その文自体が根拠として引用されている必要があります。
  たとえば `For this repository, the repository-level policy is:` に続く制約文が対象です。
  task 固有の制約は task scope に残ります。
  その他の通常の制約も導出された task、workspace、execution の scope にとどまり、`supported` として保存されます。
  operation に結び付く場合も execution scope の識別子として扱います。
- user の決定を直接引用した場合、その決定は導出された task、workspace、execution の scope で `current` になります。
- fact、observation、failure pattern、agent の主張は `supported` にとどまります。
  task の完了や agent の「tests passed」という報告は、実行状態または報告内容の根拠であり、要求された変更の正しさを検証した証拠ではありません。

実 model の呼び出しが成功し、response 全体の検証が終わった後、Worker は repository declaration block から正確な clause を決定的に投影します。
最初の非空行は parser が認識する header で始まり、同じ行または次の行に直接の制約文が続く必要があります。
たとえば `For this repository, the repository-level policy is:` と `For this repository, the repository-level policy has changed:` を認識します。
直後に続く `Open question:` の行は任意で、その次に直結する `Previous repository-level policy to replace:` の行も任意です。
これらは同じ block の一部です。
引用または code fence 内の clause、4つの space または tab で始まる行、未対応の header は受理しません。
説明文が現れると block はそこで終了し、後続の header や predecessor は無視します。
predecessor は supersession のための入力専用 authority hint であり、knowledge claim にはなりません。

たとえば Task A の declaration prefix は次のとおりです。

```text
For this repository, the repository-level policy is:
Report output format must be JSON.
Open question: The required report field set remains undecided.
```

Task B で制約を変更する declaration prefix は次のとおりです。

```text
For this repository, the repository-level policy has changed:
Report output format must be TOML.
Open question: The required report field set remains undecided.
Previous repository-level policy to replace: Report output format must be JSON.
```

Task B に Task A の制約文を正確に指定すると supersession が可能です。
predecessor text 自体は claim として保存しません。
repository-level と明示された制約は `current` になり、unresolved clause は `supported` として保存され、resolver は `supported` または `current` のものを返せます。
task 固有の記述を repository-wide policy にはしません。
provider failure または不正な model output を決定的な投影で補うことはできず、どちらの場合も projection を commit せず checkpoint を進めません。
追加の extractor 提案は model-derived として区別し、同じ support、scope、output の検証を通します。

変更された repository policy が以前の current policy を supersede できるのは、新しい instruction が `Previous repository-level policy to replace: ...` に旧文を正確に指定した場合、または旧 policy の直接根拠がすべて同じ host と session の instruction であり、新しい変更指示がその source revision より後の場合です。
どちらの条件も満たさなければ、旧 item は `current` のまま残り、競合する新 item は `supported` として保存されます。
Worker は最終書き込み優先で旧 policy を置き換えません。
生成 summary は元の引用を繰り返し、summary provenance として記録するため、独立した根拠には数えません。

knowledge item ごとの direct または summary support provenance は異なる参照を最大16件保存します。
有効な support reference が上限を超えた場合、Worker は上限内の参照を保存し、sticky な `support_incomplete` flag を設定します。
migration も、既存の support row が上限を超える item にこの flag を設定します。
保存した参照を完全な根拠履歴として扱いません。
`context_resolve` は該当 item に `support_incomplete` を含め、summary では `knowledge_summary_support_incomplete` を返します。
選択された knowledge または supersession history の根拠が不完全なら `context.partial.value` を `true` にし、reason に `knowledge_support_incomplete` を加えます。
根拠が不完全な旧 policy は、source revision の順序だけでは supersede できません。
認証済みの同じ scope の変更指示が旧文を正確に指定すれば、その指示による supersession は可能です。

既定の sync policy は instruction と error の preview を送らないため、設定済みで処理が追いついた Worker でも、active な `supported` または `current` knowledge item がなければ `ready_empty` になります。
これは知識が空の projection を正常に作成した状態であり、`disabled`、`not_configured`、`failed`、`lagging` とは異なります。
`context_resolve` は引き続き observation を返せます。
bounded preview を opt-in すると抽出可能な本文が増えますが、preview に秘密が含まれる可能性は前述のとおりです。

`source_head_revision` と `source_acked_revision` は source ごとの journal cursor です。
`latest_cloud_seq` は別の D1 cursor なので、同じ番号体系として比較しないでください。
gap または未 ACK の source revision がある場合、observation context は partial として返ります。

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

## Dashboard

同じ Worker の `/dash/` に Access で保護された read-only dashboard を配置します。authority、freshness、Host 側の前提、deploy 後の検証は [dashboard の運用文書](fabric-dashboard.md) を参照してください。

## Development

local Worker 開発では `fabric/.dev.vars.example` を `fabric/.dev.vars` にコピーします。`.dev.vars`、Worker secret、Access service-token secret、host bearer token、endpoint environment file は commit しないでください。

routed tool schema と MCP protocol version は Rust 生成の contract snapshot に対して Rust/Node の両テストで照合します。`serverInfo.version` は Temote CLI CalVer ではなく、`GATEWAY_DEPLOYMENT` version-metadata binding が供給する Cloudflare deployment revision です。
