# local-mcp

`local-mcp` exposes basic local-machine capabilities as MCP tools: file reads,
image reads, directory listings, sandboxed file writes and commands, plus explicitly approved
unsandboxed command execution. It intentionally does not provide web search or a
dedicated network-request tool.

Commands are isolated with OpenAI Codex's `codex-rs/sandboxing`: Landlock and the
Linux sandbox helper on Linux, and Seatbelt (`sandbox-exec`) on macOS. Network
access is denied for ordinary commands.

## Usage

```sh
cargo build --release

# Run one persistent MCP server (for example through a tunnel):
local-mcp mcp

# In another terminal, start a session in the project directory:
cd ./some-project
local-mcp start

# Or choose a stable session ID (letters, numbers, "-", "_", and "."):
local-mcp start my-project

# Give the printed session ID to the agent in your prompt. The agent includes it
# in each local-mcp tool call.

# In the approvals UI, allow every unsandboxed call for the session:
/permissions yolo

# Manage the current session from the start screen:
/permission ask
/permission yolo
/permission allow ../another-project
/permission revoke ../another-project
/permission list
/permission status
```

## Remote clients over HTTP

`local-mcp serve` runs the same MCP server over HTTP behind OAuth, for clients
that can only reach a public HTTPS URL, such as ChatGPT custom connectors:

```sh
# Terminal 1: the server, behind a tunnel that terminates TLS for the public URL.
local-mcp serve --public-url https://local-mcp.example.com

# Terminal 2: the tunnel.
cloudflared tunnel --url http://127.0.0.1:8791 run my-tunnel

# Terminal 3: a session, as usual.
cd ./some-project
local-mcp start my-project
```

Register `https://local-mcp.example.com/mcp` in the client and pick OAuth. The
client registers itself (RFC 7591), the browser lands on an approval page, and
the **admin token** printed by `local-mcp serve` authorizes the grant. Access
tokens last an hour and are renewed with rotating refresh tokens.

Only redirect URIs on the allow list may register. ChatGPT and Claude are
allowed by default; add more with `--allow-redirect-prefix` (a trailing `/`
makes the value a prefix). The admin token comes from `--admin-token`,
`LOCAL_MCP_OAUTH_ADMIN_TOKEN`, or a generated file in the state directory;
delete `oauth-admin-token` there to roll it, and `oauth.json` to drop every
issued token.

The endpoint publishes `/.well-known/oauth-protected-resource` and
`/.well-known/oauth-authorization-server` (also under `/mcp`), and answers
unauthenticated MCP calls with `401` and a `WWW-Authenticate` challenge.
`--addr` defaults to `127.0.0.1:8791`, so nothing is exposed beyond the tunnel.
Remember that a reachable connector can read files and run sandboxed commands
under the session's roots, so keep `/permission ask` and stop the tunnel when
it is not in use.

With Nix, `curl` and `bash` are included in the runtime environment. Linux builds
also include `bwrap`:

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
Every tool takes a `session_id`. The agent can call `session_info` with the ID
from the prompt to confirm the working directory and sandbox roots. One
`local-mcp mcp` or `local-mcp serve` process can therefore serve multiple
independently configured sessions.
`get_image` returns PNG, JPEG, GIF, WebP, BMP, TIFF, and AVIF files as native MCP
image content. Relative image paths are resolved from the session working directory.

Each session uses its own permission-restricted Unix domain socket. Both the MCP
server and the start UI block on I/O, so idle operation and pending approvals
do not use polling timers.

The `start` screen also receives live activity from MCP calls. It shows file and
image reads, directory listings, file edits with unified diffs and line counts,
and command start/completion with output, in a compact Codex-style timeline.
`execute` returns its normal result for commands that finish within 30 seconds.
Longer commands continue in the background and return a `job_id`; use `poll_job`
to check for completion or `stop_job` to terminate them. Use `start_command`
when a command should run in the background immediately without the 30-second
foreground wait.

On Linux, the build produces `local-mcp` and its sibling `codex-linux-sandbox`;
install or copy both into the same directory, and ensure `bwrap` (bubblewrap) is
available in `PATH`. On macOS, only `local-mcp` is needed; sandboxed commands use
the system `/usr/bin/sandbox-exec`. Windows support is not implemented yet.
