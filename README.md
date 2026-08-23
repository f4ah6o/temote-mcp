# Temote MCP

[日本語](README.ja.md)

Temote MCP exposes local files, commands, and selected host integrations as MCP tools while keeping normal sessions sandboxed and approval-aware.

## Install

Prebuilt binaries are available through `cargo-binstall`:

```sh
cargo binstall temote-mcp
temote-mcp doctor
```

To build from source instead:

```sh
cargo install temote-mcp --locked
```

When replacing a legacy `just up` deployment, install the new binary first, inspect the migration, apply it, and then start the current supervisor:

```sh
cargo binstall temote-mcp --force
temote-mcp migrate --dry-run
temote-mcp migrate
temote-mcp up --profile cloudflare
```

`migrate` handles both legacy runtime ownership and compatible legacy Cloudflare configuration. It never overwrites an existing `public.env` or a different existing Tunnel token, copies only supported Temote Cloudflare/runtime keys from checkout-local `.env`, and does not stop independently started local sessions. `temote-mcp up --profile cloudflare` can also bootstrap the compatible configuration migration when the destination is still missing.

On macOS, the canonical default is `~/.config/temote-mcp/public.env`. A file left at the previous accidental `~/Library/Application Support/temote-mcp/public.env` location is recognized as a migration source. Linux continues to use its normal config directory semantics, and `TEMOTE_MCP_ENV_FILE` remains the explicit override.

Apple Silicon macOS and Linux are supported. Intel macOS and native Windows are not supported; WSL2 can be used for the gateway endpoint path.

## Start sessions

For an always-on host, configure a named root and run the HTTP supervisor:

```sh
# Example host layout:
# ~/src -> /Volumes/devstorage/Developer
export TEMOTE_MCP_ROOTS='src=~/src'
# Existing deployments default to the Cloudflare profile.
temote-mcp up --profile cloudflare
# Or use Tailscale Funnel + Temote local OAuth:
# temote-mcp up --profile tailscale
# Or bootstrap and use an outbound-only OpenAI Secure MCP Tunnel.
# Both commands prompt for the required API key without terminal echo when the
# corresponding environment variable is absent:
# temote-mcp openai setup --workspace-id <workspace-id>
# temote-mcp up --profile openai
```

An authenticated MCP client can then use:

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
```

Managed sessions are always normal sandboxed sessions. `session_start` accepts only named-root-relative paths and cannot enable yolo mode; host/network-sensitive operations still require approval in the local `temote-mcp up` terminal. Stop this supervisor with `temote-mcp down`.

For a traditional local session, run:

```sh
cd ~/src/my-project
temote-mcp start my-project
```

Local stdio clients can launch `temote-mcp mcp`. A deliberately unrestricted CLI session remains available with `temote-mcp start my-project --yolo`.

## Agent skill

Temote MCP ships an Agent Skill that teaches compatible coding agents how to use sessions, Git tools, background jobs, and bridged MCP servers effectively.

```sh
gh skill install f4ah6o/temote-mcp temote-mcp --scope user
```

Specify `--agent codex`, `--agent claude-code`, or another supported agent when needed.

## More documentation

- [Using sessions and tools](docs/usage.md)
- [Managed sessions and named roots](docs/managed-sessions.md)
- [Remote connection profiles: Cloudflare, Tailscale, or OpenAI Secure MCP Tunnel](docs/public-http.md)
- [1Password and kintone integrations](docs/integrations.md)
- [Multi-host Cloudflare gateway](docs/gateway.md)
- [Linux sandbox and crates.io packaging](docs/linux-sandbox.md)
- [Building, testing, and releasing](docs/development.md)

In a repository checkout, `just up` and `just down` are development wrappers that build or select the checkout binary and delegate to these commands. Installed users do not need `just`.

Repository-specific instructions for coding agents are in [AGENTS.md](AGENTS.md).

## Origin and license

This project is derived from [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp). The name **Temote** draws on [@mr_konn's proposal of 「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46), coined as the opposite of “remote.” See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for attribution details.

Licensed under MIT and Apache-2.0 as described in the repository license files.
