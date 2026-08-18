# Temote MCP

[日本語](README.ja.md)

Temote MCP exposes local files, commands, and selected host integrations as MCP tools while keeping normal sessions sandboxed and approval-aware.

## Install

```sh
cargo install --git https://github.com/f4ah6o/temote-mcp --locked
temote-mcp doctor
```

macOS and Linux are supported. Native Windows is not supported; WSL2 can be used for the gateway endpoint path.

## Start a session

Run Temote MCP from the directory the agent should work in:

```sh
cd ~/src/my-project
temote-mcp start my-project
```

Then configure an MCP client to start the local stdio server with:

```sh
temote-mcp mcp
```

Every tool call except `session_list` uses a session ID. The session terminal shows approvals and lets you change permitted directories:

```text
/permission allow ../another-project
/permission revoke ../another-project
/permission list
```

For an intentionally unrestricted session:

```sh
temote-mcp start my-project --yolo
```

`--yolo` removes Temote MCP's path, sandbox, and local-approval boundaries. Use it only when unrestricted host access is intended.

## Agent skill

Temote MCP ships an Agent Skill that teaches compatible coding agents how to use sessions, Git tools, background jobs, and bridged MCP servers effectively.

```sh
gh skill install f4ah6o/temote-mcp temote-mcp --scope user
```

Specify `--agent codex`, `--agent claude-code`, or another supported agent when needed.

## More documentation

- [Using sessions and tools](docs/usage.md)
- [Public HTTP endpoint and Cloudflare Access](docs/public-http.md)
- [1Password and kintone integrations](docs/integrations.md)
- [Multi-host Cloudflare gateway](docs/gateway.md)
- [Building, testing, and releasing](docs/development.md)

Repository-specific instructions for coding agents are in [AGENTS.md](AGENTS.md).

## Origin and license

This project is derived from [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp). The name **Temote** draws on [@mr_konn's proposal of 「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46), coined as the opposite of “remote.” See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for attribution details.

Licensed under MIT and Apache-2.0 as described in the repository license files.
