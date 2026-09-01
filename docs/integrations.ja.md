# MCP integration

[English](integrations.md)

Temote MCP は、一部の local MCP server と secret injection workflow を bridge し、credential を選択した session process 内に保持します。

## 1Password Environments MCP

1Password の Labs/Developer settings で **MCP Server** を有効にします。Temote MCP は公式 executable を次の場所から探します。

- macOS: `/Applications/1Password.app/Contents/MacOS/1password-mcp`
- Linux: `/opt/1Password/1password-mcp`

標準外の場所にある場合だけ `TEMOTE_MCP_ONEPASSWORD_MCP` に absolute path を指定します。

利用順序:

1. `onepassword_mcp_discover` で resource / tool schema を取得
2. `onepassword_mcp_read_resource` で child server が公開した document/resource を読む
3. `onepassword_mcp_call` で child tool を呼ぶ

通常 session では read-only と明示されていない child tool を approval-gate します。approval summary に argument value は永続化しません。

### 公式 CLI による item batch read

`onepassword_item_get` は general 1Password item を公式 `op` CLI 経由で読み、複数 item を1回の upstream `op item get -` にまとめます。`items` には正確な item ID または title を指定します。同名 title の曖昧性を避けるには `vault`、複数 account がある host では必要に応じて `account` を指定します。

bridge はまず公式 CLI で item overview を解決し、同じ item ID の重複を除去してから batch fetch します。返却 JSON には secret value が含まれ得るため、read-only operation ですが通常 session では local approval を要求します。approval/activity には件数と scope 設定有無だけを出し、item title や取得値は記録しません。

この経路は公式 1Password の authentication / encryption / synchronization / cache semantics を維持し、local 1Password database を書き換えません。将来 direct local-DB read を追加する場合も explicit opt-in の read-only optimization とし、mutation は公式 1Password 経路に限定します。

### 1Password service-account mode

service-account token を渡して session を起動します。

```sh
OP_SERVICE_ACCOUNT_TOKEN='<service-account-token>' temote-mcp start my-project
```

session process が token を memory に保持し、session JSON へ書かず、MCP tool から返しません。

- `onepassword_service_account_status`: token の有無と `op whoami` を token 値を返さず確認
- `onepassword_service_account_run`: `op run` 経由で command 実行

secret は `environment` に `op://...` reference として渡すか、`env_files` で checked-in template を指定します。`environment` の plaintext secret は拒否します。1Password CLI の output masking は維持され、target command は `OP_SERVICE_ACCOUNT_TOKEN` を直接継承しません。

通常 session では host approval が必要です。`--yolo` はこの Temote MCP approval boundary を外します。

## kintone MCP Server

公式 server を host に install します。

```sh
npm install -g @kintone/mcp-server
```

credential は `serve` や `gateway-agent` ではなく **`temote-mcp start` process** に渡します。

```sh
KINTONE_BASE_URL='https://example.cybozu.com' \
KINTONE_API_TOKEN='<api-token>' \
temote-mcp start my-project
```

`KINTONE_USERNAME` / `KINTONE_PASSWORD` による username/password 認証も使えます。upstream が対応する Basic auth、PFX certificate、proxy、attachment directory の設定も bridge します。

通常 session では `KINTONE_PFX_FILE_PATH` と `KINTONE_ATTACHMENTS_DIR` を permitted root 内に置く必要があります。server が `PATH` にない場合だけ `TEMOTE_MCP_KINTONE_MCP` に absolute executable path を指定します。

利用順序:

1. `kintone_mcp_status` で executable/configuration の有無を credential/tenant URL を返さず確認
2. `kintone_mcp_discover` で現在の tool schema を取得
3. `kintone_mcp_call` で discover 済み tool を呼ぶ

upstream server が全 tool を read-only/mutating と annotate していないため、通常 Temote MCP session では forwarded kintone call を approval-gate します。

child process に渡す environment は最小 runtime environment と allow-list 済み kintone setting に限定されます。`temote-mcp start` が持つその他の credential は自動継承しません。

## cli-kintone による補完

MCP server でまだ扱いにくい API-backed operation、特に attachment 付き bulk record、guest space の record、JavaScript/CSS customization の export/apply、plugin upload を使う場合は公式 CLI を install します。

```sh
npm install -g @kintone/cli
```

`temote-mcp start` は同じ `KINTONE_BASE_URL`、`KINTONE_USERNAME` / `KINTONE_PASSWORD`、`KINTONE_API_TOKEN`、Basic auth、proxy、`KINTONE_GUEST_SPACE_ID` を capture します。`cli-kintone` が標準外の場所にある場合だけ `TEMOTE_MCP_KINTONE_CLI` に absolute path を指定します。

local session supervisor を使う場合も、`temote-mcp start` / `temote-mcp session start` は allow-list 済みの integration environment を owner-only control socket 経由で対象 session runtime に渡します。credential は supervisor の process environment、session metadata、lifecycle state には保存しません。`session restart` でも呼び出し元 environment を再 capture するため、credential を注入していた session は restart 時にも同じ secret-injection wrapper から実行してください。

最初に `kintone_cli_status` を使います。`kintone_cli_run` は API を使う次の command pair だけを受け付けます。

- `record export`
- `record import`
- `record delete`
- `customize export`
- `customize apply`
- `plugin upload`

agent が渡す argument では `--base-url`、`--username`、`--password`、`--api-token`、proxy、PFX、`--guest-space-id` などの connection/auth/target option を拒否し、credential と tenant/guest-space target を session process に固定します。現行 cli-kintone は PFX password を command-line option でしか受け取らないため、CLI bridge では PFX を転送しません。PFX 認証が必要な場合は kintone MCP bridge を使います。

`--attachments-dir`、`--file-path`、`--input`、`--output` など path を取る option は指定 cwd 基準で解決し、通常 session では permitted root 内に限定します。customize manifest 内の local JS/CSS reference も事前検証し、attachment import では parent traversal と attachment tree 内の symlink を拒否します。大きな `record export` の CSV は `stdout_path` で atomically file 保存できます。`kintone_cli_run` は通常 mode ではすべて local approval が必要です。CLI child process には `temote-mcp start` の全 environment ではなく、allow-list 済み runtime/kintone environment だけを渡します。
