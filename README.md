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

For an always-on host, configure a named root, run one lifecycle supervisor, then start the HTTP ingress separately:

```sh
# Example host layout:
# ~/src -> /Volumes/devstorage/Developer
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor

# From another terminal/service. Existing deployments default to Cloudflare.
temote-mcp up --profile cloudflare
# Or use Tailscale Funnel + Temote local OAuth:
# temote-mcp up --profile tailscale
# Or bootstrap and use an outbound-only OpenAI Secure MCP Tunnel.
# Both commands prompt for the required API key without terminal echo when the
# corresponding environment variable is absent:
# temote-mcp openai setup --workspace-id <workspace-id>
# temote-mcp up --profile openai
```

Direct `temote-mcp up` ingress is **single-host per public endpoint**. Set `TEMOTE_MCP_HOST_ID=ubuntu1` (or another stable non-secret identifier) to make host ownership explicit in startup, `doctor`, supervisor, and session diagnostics; when unset, Temote falls back to the OS hostname. Do not run the same Cloudflare Tunnel token/hostname concurrently on multiple Temote hosts as direct-ingress replicas: Cloudflare routing is not Temote-session-aware, while session state remains host-local. For one public endpoint spanning multiple hosts, use `temote-mcp gateway-agent` with the Worker/Durable Objects gateway described in [multi-host Cloudflare gateway](docs/gateway.md).

An authenticated MCP client can then use:

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
```

Managed sessions are always normal sandboxed sessions. `session_start` accepts only named-root-relative paths and cannot enable yolo mode. HTTP `serve/up` delegates session ownership and Tailscale OAuth approval to the local lifecycle supervisor over its owner-only Unix socket. Use `temote-mcp session console` for approvals. `temote-mcp down` stops only the HTTP origin/managed ingress; the lifecycle supervisor and its sessions remain alive.

For local sessions, run one Temote session supervisor and manage runtimes through it:

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor

# From another terminal:
temote-mcp session start my-project --path src/my-project
temote-mcp session list
temote-mcp session info my-project
temote-mcp session permission my-project status
temote-mcp session permission my-project allow /path/to/extra-root
temote-mcp session restart-policy my-project on-failure
temote-mcp session console
temote-mcp session stop my-project
```

The approval console is an attachment, not the runtime owner. Closing its terminal or sending stdin EOF leaves session runtimes alive; approval-required operations fail closed until a console reconnects. Lifecycle metadata records `starting`, `active`, `stopping`, `stopped`, and `crashed`, including crash reason and last error. `session list` probes the socket and never reports dead runtime metadata as `active`. Detached permission changes use the owner-only supervisor socket and do not restart the runtime. Restart policy defaults to `never`; explicit `on-failure` uses bounded exponential backoff with a five-attempt limit and records restart count/timestamps/limit reason. Captured start credentials remain memory-only, so a supervisor process restart never silently resumes credential-bearing automatic restarts; use explicit `session restart` after such a supervisor restart.

For compatibility, `cd ~/src/my-project && temote-mcp start my-project` remains a local-supervisor shorthand for starting the current directory. It requires `temote-mcp supervisor` to be running. `--yolo` remains available only on this local CLI path; remote MCP `session_start` cannot create yolo sessions. Local stdio clients can launch `temote-mcp mcp`.

## Codex plugin and Agent Skill

For an installed Temote binary, the normal local Codex path is binary-owned plugin installation:

```sh
temote-mcp codex plugin install
temote-mcp codex status
temote-mcp codex diagnose --json
```

The installer writes the plugin under `CODEX_HOME` (or `~/.codex`), enables `temote-mcp@debug` in Codex configuration, and pins the exact Temote executable that performed the install in both the generated MCP configuration and `.temote-mcp-bin`. It does not silently fall back to a different ambient `temote-mcp` on `PATH`. After upgrading Temote, run `temote-mcp codex plugin install` again so the installed plugin moves to the new binary/version. Remove it with `temote-mcp codex plugin uninstall`. Restart an already-running Codex session after install or uninstall so its loaded plugin inventory matches disk. Install and uninstall are serialized and transactional: a complete validated bundle is atomically swapped into place, Codex config is replaced atomically without following symlinks, and uninstall disables config before bundle cleanup. `codex status --json` reports recoverable transaction artifacts, dangling config, disabled bundles, and stale versions.

The repository root remains a directly inspectable local Codex plugin for development: `.codex-plugin/plugin.json` exposes the existing `skills/temote-mcp` guidance and `.mcp.json` launches `temote-mcp mcp` from `PATH`.

The plugin is intentionally thin: session lifecycle, named-root resolution, sandboxing, approvals, OAuth, and ingress remain owned by the native Temote binary. Start `temote-mcp supervisor` normally before using local managed sessions. ChatGPT and other remote clients continue to use the Cloudflare, Tailscale, or OpenAI Secure MCP Tunnel profiles rather than the local stdio plugin path.

For coding agents that consume Agent Skills without Codex plugins, install the same bundled Skill directly:

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
