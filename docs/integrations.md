# MCP integrations

[日本語](integrations.ja.md)

Temote MCP can bridge selected local MCP servers and secret-injection workflows while keeping their credentials in the selected session process.

## 1Password Environments MCP

Enable **MCP Server** in 1Password Labs/Developer settings. Temote MCP looks for the official executable at:

- macOS: `/Applications/1Password.app/Contents/MacOS/1password-mcp`
- Linux: `/opt/1Password/1password-mcp`

Set `TEMOTE_MCP_ONEPASSWORD_MCP` only for a non-standard absolute path.

Use the bridge in this order:

1. `onepassword_mcp_discover` to list resources and tool schemas.
2. `onepassword_mcp_read_resource` for documentation/resources advertised by the child server.
3. `onepassword_mcp_call` for a named child tool.

Normal sessions approval-gate child tools that are not marked read-only. Argument values are not persisted in approval summaries.

### 1Password service-account mode

Start the session with a service-account token:

```sh
OP_SERVICE_ACCOUNT_TOKEN='<service-account-token>' temote-mcp start my-project
```

The session process holds the token in memory; it is not written to session JSON or returned by MCP tools.

- `onepassword_service_account_status` checks token presence and `op whoami` without returning the token.
- `onepassword_service_account_run` runs a command through `op run`.

Provide secret values as `op://...` references through `environment`, or use checked-in env templates through `env_files`. Plaintext secret values in `environment` are rejected. 1Password CLI output masking remains enabled and the target command does not directly inherit `OP_SERVICE_ACCOUNT_TOKEN`.

Normal sessions still require host approval. `--yolo` removes that Temote MCP approval boundary.

## kintone MCP Server

Install the official server on the host:

```sh
npm install -g @kintone/mcp-server
```

Pass credentials to the **`temote-mcp start` process**, not to `serve` or `gateway-agent`:

```sh
KINTONE_BASE_URL='https://example.cybozu.com' \
KINTONE_API_TOKEN='<api-token>' \
temote-mcp start my-project
```

Username/password authentication is also supported with `KINTONE_USERNAME` and `KINTONE_PASSWORD`. The bridge also passes the upstream server's optional Basic-auth, PFX certificate, proxy, and attachment-directory settings.

`KINTONE_PFX_FILE_PATH` and `KINTONE_ATTACHMENTS_DIR` must stay inside permitted roots in normal sessions. Set `TEMOTE_MCP_KINTONE_MCP` to an absolute executable path only when the server is not on `PATH`.

Use the bridge in this order:

1. `kintone_mcp_status` checks executable/configuration presence without returning credentials or tenant URL.
2. `kintone_mcp_discover` lists the child server's current tool schemas.
3. `kintone_mcp_call` invokes a discovered tool.

Because the upstream server does not currently annotate every tool as read-only versus mutating, forwarded kintone calls are approval-gated in normal Temote MCP sessions.

The child process receives only a small runtime environment plus allow-listed kintone settings. Other credentials inherited by `temote-mcp start` are not passed through automatically.
