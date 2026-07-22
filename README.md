# local-mcp

`local-mcp` exposes basic local-machine capabilities as MCP tools: file reads,
directory listings, sandboxed file writes and commands, plus explicitly approved
unsandboxed command execution. It intentionally does not provide web search or a
dedicated network-request tool.

Commands are isolated with OpenAI Codex's `codex-rs/sandboxing` and Linux sandbox
helper. Network access is denied for ordinary commands.

## Usage

```sh
cargo build --release

# Add this command as a stdio MCP server (the `mcp` argument is optional):
local-mcp mcp

# In another terminal, approve unsandboxed calls:
local-mcp approvals

# In the approvals UI, allow every unsandboxed call until it exits:
/permissions yolo

# Trust sandboxed writes/commands rooted in a directory without prompting:
local-mcp permit ./some-project
local-mcp permits
local-mcp revoke ./some-project

# Set the cwd used when tools omit cwd and for relative file paths:
local-mcp set-cwd ./some-project
```

With Nix, `bwrap` and `curl` are included in the runtime environment:

```sh
nix run github:OWNER/local-mcp
nix develop
nix build
```

Sandboxed calls are always allowed and have no network access. `without_sandbox`
runs with the service user's full host permissions and network access, so it asks
the approvals process before every call. `/permissions yolo` disables those
prompts only for the lifetime of that approvals process; `/permissions ask`
turns prompts back on. The singular `/permission ...` spelling is also accepted.

Approval requests use a permission-restricted Unix domain socket. Both the MCP
server and the approval UI block on I/O, so idle operation and pending approvals
do not use polling timers.

The build produces `local-mcp` and its sibling `codex-linux-sandbox`; install or
copy both into the same directory. On Linux, `bwrap` (bubblewrap) must be
available in `PATH`. macOS/Windows support is not implemented yet.
