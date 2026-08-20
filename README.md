# Temote MCP

[日本語](README.ja.md)

Temote MCP exposes local files, commands, and selected host integrations as MCP tools while keeping normal sessions sandboxed and approval-aware.

## Install

Prebuilt binaries are available through `cargo-binstall`:

```sh
cargo binstall --git https://github.com/f4ah6o/temote-mcp temote-mcp
temote-mcp doctor
```

To build from source instead:

```sh
cargo install --git https://github.com/f4ah6o/temote-mcp --locked
```

Apple Silicon macOS and Linux are supported. Intel macOS and native Windows are not supported; WSL2 can be used for the gateway endpoint path.

## Start sessions

For an always-on host, configure a named root and run the HTTP supervisor:

```sh
# Example host layout:
# ~/src -> /Volumes/devstorage/Developer
export TEMOTE_MCP_ROOTS='src=~/src'
just up
```

An authenticated MCP client can then use:

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
```

Managed sessions are always normal sandboxed sessions. `session_start` accepts only named-root-relative paths and cannot enable yolo mode; host/network-sensitive operations still require approval in the local `just up` terminal.

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
- [Public HTTP endpoint and Cloudflare Access](docs/public-http.md)
- [1Password and kintone integrations](docs/integrations.md)
- [Multi-host Cloudflare gateway](docs/gateway.md)
- [Building, testing, and releasing](docs/development.md)

Repository-specific instructions for coding agents are in [AGENTS.md](AGENTS.md).

## Origin and license

This project is derived from [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp). The name **Temote** draws on [@mr_konn's proposal of 「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46), coined as the opposite of “remote.” See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for attribution details.

Licensed under MIT and Apache-2.0 as described in the repository license files.
