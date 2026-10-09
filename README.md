# Temote MCP

[日本語](README.ja.md)

Temote MCP delegates local-machine work to coding agents through explicit, sandboxed sessions. Codex app-server, OpenCode serve, Devin ACP, and Devin Cloud share retained tasks, typed controls, and bounded evidence. Temote Fabric connects multiple Hosts through one authenticated MCP endpoint.

## Install

Build the current source with Rust:

```sh
cargo install --path . --locked
temote doctor
```

[Prebuilt releases](https://github.com/f4ah6o/temote-mcp/releases) are also available through `cargo-binstall`: run `cargo binstall temote-mcp`, or pin a release with `cargo binstall temote-mcp@<version>`. The crates.io package remains `temote-mcp`; it installs the canonical `temote` command and the compatible `temote-mcp` executable alias. See [migration](docs/naming-migration.md).

## First session

Configure a named root in the supervisor's launch environment, then start inside it:

```sh
export TEMOTE_ROOTS='src=~/src'
cd ~/src/your-project
temote start work
```

New sessions use `agent` permission mode and require a configured named root. Set roots before the supervisor starts; an existing supervisor keeps its configured roots. Delegated tasks remain restricted to the session's canonical scope. Use `--yolo` only when you intentionally want to remove Temote's local boundaries.

Install the bundled Codex plugin:

```sh
temote codex plugin install
```

For other Agent Skill clients:

```sh
gh skill install f4ah6o/temote-mcp temote-mcp --scope user
```

Select the session, probe the backend, start a task with a fresh `operation_id`, and read its retained state and scoped evidence. The [usage guide](docs/usage.md) covers the full lifecycle.

## Documentation

- [Sessions and task tools](docs/usage.md)
- [Managed sessions and named roots](docs/managed-sessions.md)
- [Authenticated MCP and ingress](docs/public-http.md)
- [Temote Fabric](docs/gateway.md)
- [Build, tests, and release](docs/development.md)
- [Repository agent instructions](AGENTS.md)

## Attribution and license

This project derives from [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp). **Temote** takes its name from [@mr_konn's proposal of 「テモート」 as the opposite of remote](https://x.com/mr_konn/status/1318116448519114752?s=46). See [third-party notices](THIRD_PARTY_NOTICES.md). The repository is licensed under MIT and Apache-2.0.
