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
