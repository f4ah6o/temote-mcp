# local-mcp

`local-mcp` exposes basic local-machine capabilities as MCP tools: file reads,
directory listings, sandboxed file writes, sandboxed command execution, and
explicitly approved HTTP requests. It intentionally does not provide web search.

Commands are isolated with OpenAI Codex's `codex-rs/sandboxing` and Linux sandbox
helper. Network access is denied for ordinary commands.

## Usage

```sh
cargo build --release

# Add this command as a stdio MCP server (the `mcp` argument is optional):
local-mcp mcp

# In another terminal, handle calls which need one-time approval:
local-mcp approvals

# Trust sandboxed writes/commands rooted in a directory without prompting:
local-mcp permit ./some-project
local-mcp permits
local-mcp revoke ./some-project
```

With Nix, `bwrap` and `curl` are included in the runtime environment:

```sh
nix run github:OWNER/local-mcp
nix develop
nix build
```

Permanent permits are canonical directory paths stored under the platform's
local state directory. They do not enable network access. `network_request`
always asks the `approvals` process and executes `curl` with network enabled only
for that one sandboxed process.

Approval requests use a permission-restricted Unix domain socket. Both the MCP
server and the approval UI block on I/O, so idle operation and pending approvals
do not use polling timers.

The build produces `local-mcp` and its sibling `codex-linux-sandbox`; install or
copy both into the same directory. On Linux, `bwrap` (bubblewrap) must be
available in `PATH`. macOS/Windows support is not implemented yet.
