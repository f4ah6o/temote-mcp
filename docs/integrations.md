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

### Batched item reads with the official CLI

`onepassword_item_get` reads general 1Password items through the official `op` CLI and batches multiple requested items into one upstream `op item get -` call. Supply exact item IDs or exact titles; use `vault` to disambiguate repeated titles and `account` when the host has multiple 1Password accounts.

The bridge first resolves item overviews with the official CLI, removes duplicate resolved item IDs, and then performs one batch fetch. Concurrent reads in the same session and `(account, vault)` scope are held for a bounded 15 ms micro-batch window and share one list/get pair up to the 100-item limit. Results are fanned back out by resolved item ID; a canceled or invalid caller does not cancel or poison unrelated callers. No secret plaintext is stored in a durable cache. The returned JSON can contain secret values. Normal sessions therefore require local approval even though the operation is read-only. Approval/activity text reports only counts and whether a scope was configured; item titles and returned values are not logged.

This path keeps the official 1Password authentication, encryption, synchronization, and cache semantics. It does not write the local 1Password database. Direct local-database reads, if added later, remain an explicit opt-in read-only optimization; mutations continue to use official 1Password paths.

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

## cli-kintone complement

Install the official CLI when you need API-backed operations that the MCP server does not currently cover well, especially bulk record workflows with attachments, guest-space record access, JavaScript/CSS customization export/apply, or plugin upload:

```sh
npm install -g @kintone/cli
```

The same `KINTONE_BASE_URL`, `KINTONE_USERNAME` / `KINTONE_PASSWORD`, `KINTONE_API_TOKEN`, Basic-auth, proxy, and `KINTONE_GUEST_SPACE_ID` settings are captured by `temote-mcp start`. Set `TEMOTE_MCP_KINTONE_CLI` only when `cli-kintone` is installed at a non-standard absolute path.

When using the local session supervisor, `temote-mcp start` / `temote-mcp session start` forwards only the allow-listed integration environment to the target session runtime over the owner-only control socket. Credentials are not placed in the supervisor process environment, session metadata, or lifecycle state. `session restart` captures the caller environment again, so sessions that use secret-injection wrappers must be restarted through the same wrapper.

Use `kintone_cli_status` first. `kintone_cli_run` then accepts only these API-backed command pairs:

- `record export`
- `record import`
- `record delete`
- `customize export`
- `customize apply`
- `plugin upload`

Connection/authentication and target options such as `--base-url`, `--username`, `--password`, `--api-token`, proxy, PFX, and `--guest-space-id` are rejected in agent-supplied arguments so credentials and the tenant/guest-space target stay pinned to the session process. PFX is intentionally not forwarded through the CLI bridge because current cli-kintone exposes its certificate password only as a command-line option; use the kintone MCP bridge when PFX authentication is required.

Path-bearing CLI options such as `--attachments-dir`, `--file-path`, `--input`, and `--output` are resolved against the requested working directory and must remain inside permitted roots in normal sessions. Customize manifests are preflighted so local JS/CSS references cannot escape those roots; attachment imports reject parent traversal and symlinks in the attachment tree. `stdout_path` can atomically save stdout, which is useful for large `record export` CSV output. All `kintone_cli_run` calls require local approval in normal mode. The CLI child receives only allow-listed runtime/kintone environment variables rather than the full `temote-mcp start` environment.
