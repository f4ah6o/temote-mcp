# local-mcp

`local-mcp` exposes basic local-machine capabilities as MCP tools: file reads,
image reads, directory listings, sandboxed file writes and commands, plus explicitly approved
unsandboxed command execution. It intentionally does not provide web search or a
dedicated network-request tool.

Commands are isolated with OpenAI Codex's `codex-rs/sandboxing` and Linux sandbox
helper. Network access is denied for ordinary commands.

## Usage

```sh
cargo build --release

# Start a session in the project directory:
cd ./some-project
local-mcp start

# Give the printed session ID to the agent and use it in the MCP command:
local-mcp mcp <SESSION_ID>

# In the approvals UI, allow every unsandboxed call until it exits:
/permissions yolo

# Manage the current session from the start screen:
/permission ask
/permission yolo
/permission allow ../another-project
/permission revoke ../another-project
/permission list
```

With Nix, `bwrap` and `curl` are included in the runtime environment:

```sh
nix run github:OWNER/local-mcp
nix develop
nix build
```

The session working directory is the directory where `local-mcp start` was run;
there is no separate persistent cwd setting. Sandboxed calls are always allowed
and have no network access. `without_sandbox`
runs with the service user's full host permissions and network access, so it asks
the approvals process before every call. `/permissions yolo` disables those
prompts only for the lifetime of that session; `/permissions ask`
turns prompts back on. The singular `/permission ...` spelling is also accepted.
The agent can call `session_info` to confirm the session ID, working directory,
and sandbox roots associated with its MCP connection.
`get_image` returns PNG, JPEG, GIF, WebP, BMP, TIFF, and AVIF files as native MCP
image content. Relative image paths are resolved from the session working directory.

Each session uses its own permission-restricted Unix domain socket. Both the MCP
server and the start UI block on I/O, so idle operation and pending approvals
do not use polling timers.

The build produces `local-mcp` and its sibling `codex-linux-sandbox`; install or
copy both into the same directory. On Linux, `bwrap` (bubblewrap) must be
available in `PATH`. macOS/Windows support is not implemented yet.
