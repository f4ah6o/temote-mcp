# local-mcp

local-mcp exposes local files and sandboxed commands as MCP tools. It does
not provide web search or a general network-request tool. Sandboxed commands
run with network access disabled. Linux uses the pinned Codex sandbox stack;
macOS uses local-mcp's native Seatbelt backend.

The public HTTP endpoint is designed for an on-demand Cloudflare Tunnel:

    ChatGPT Plus
        | Managed OAuth
        v
    Cloudflare Access -- Cloudflare Tunnel -- 127.0.0.1:8791
                                                   |
                                                   v
                                            local-mcp serve

## Build

    cargo build --release
    cargo install --path . --locked

The default build includes the public HTTP and gateway-agent commands. For a
local-only binary containing `doctor`, `start`, and `mcp` without local-mcp's
direct HTTP/JWT client dependencies, disable the default `network` feature:

    cargo build --release --no-default-features --locked

On Linux, install both local-mcp and the sibling codex-linux-sandbox binary
in the same directory, and make sure bwrap is available in PATH. macOS uses
the system Seatbelt sandbox. Windows is not supported.

Keep `--locked` when installing. The Linux Codex sandbox dependencies are
pinned to a Git revision, and resolving their transitive pre-release
dependencies without the committed lockfile can select incompatible `rama` or
`starlark` versions. macOS does not resolve the Codex sandbox crates.

Before starting a session or connecting ChatGPT, run the host diagnostics:

    local-mcp doctor

On Linux, `doctor` checks the installed `codex-linux-sandbox` helper,
`bubblewrap`, user-namespace settings, the isolated network namespace, a real
local-mcp sandbox command, and the runtime environment used by shell commands.
A failed `network namespace` check means the host cannot create the bwrap
network namespace; fix the displayed host policy hint before starting the HTTP
server. The command exits non-zero when a required check fails.

If ChatGPT reports that `/.bash_profile` or `/tmp` cannot be found or written,
update the installed binary with `just install`, run `local-mcp doctor`, and
restart the origin process. Shell commands receive a minimal environment with
`HOME` and the standard temporary directories available.

## Release versioning

Releases use CalVer `YYYY.MM.PATCH` in the `Asia/Tokyo` timezone through
[`f4ah6o/calver-action`](https://github.com/f4ah6o/calver-action). Move the
`latest` tag to a commit in `main` history to request a release:

    git tag -f latest <commit-to-release>
    git push -f origin latest

`.github/workflows/release.yaml` allocates the next prefixless CalVer tag,
updates `Cargo.toml` and `Cargo.lock` in a release-only commit, validates both
the normal and local-only builds, and pushes the immutable CalVer tag. The
release-only commit is not merged back into `main`. Existing `vYYYY.MM.PATCH`
tags are considered during allocation so migration to prefixless tags does not
reset the monthly patch counter.

## Local sessions

Start each session from the project directory whose files it may access:

    cd ~/src/local-mcp
    local-mcp start local-mcp

    cd ~/src/shuttle-rs
    local-mcp start shuttle-rs

For an explicitly unrestricted session, add `--yolo`:

    local-mcp start local-mcp --yolo

YOLO mode is intentionally dangerous: local-mcp approval prompts are skipped, path
roots are not enforced, and command tools run directly on the host with the
filesystem, environment, process, and network permissions of the user running
`local-mcp`. The mode is stored in the active session metadata so local stdio, HTTP,
and gateway requests all observe the same setting. This setting only controls
local-mcp boundaries; any confirmation or authorization enforced by an MCP client is
independent. `/permission ask` restores the normal restricted mode and
`/permission yolo` enables it again while the session runs.

The session ID is explicit in every tool call. session_list discovers active
sessions; session_info shows a session's working directory and sandbox roots.
Session IDs contain only ASCII letters, numbers, -, _, and .. A duplicate
active ID is rejected.

The initial sandbox root is the startup directory. Add another existing
directory from that session's local terminal:

    /permission allow ../another-project
    /permission revoke ../another-project
    /permission list
    /permission status

When the start process exits, its Unix socket is removed and active sandbox
jobs are cancelled. The public endpoint never exposes the separate `without_sandbox` tool. A public or
gateway request targeting a session explicitly started with `--yolo` can nevertheless
use `execute`/`start_command` with unrestricted host permissions; Cloudflare Access
(or the gateway's authentication) remains the outer authentication boundary.

For ChatGPT, use the dedicated `git_add` and `git_commit` tools to stage files
and create local commits. Use `git_fetch`, `git_pull`, and `git_push` for remote
synchronization. The remote tools execute on the host only after approval
(unless the session is in yolo mode), accept configured remote names rather
than arbitrary URLs or refspecs, disable hooks, reject force-push options, and
make `git_pull` fast-forward-only. Ordinary `execute` commands keep `.git`
metadata read-only and network access disabled.

## Public HTTP endpoint

Copy .env.example to a file outside the repository, such as
~/.config/local-mcp/public.env, and set the Cloudflare Access values. Keep
the file mode 0600; the Tunnel token is a credential.

    install -d -m 700 ~/.config/local-mcp
    cp .env.example ~/.config/local-mcp/public.env
    chmod 600 ~/.config/local-mcp/public.env
    vi ~/.config/local-mcp/public.env

Use separate terminals when the service is needed:

    # Terminal 1: the local origin
    set -a
    . ~/.config/local-mcp/public.env
    set +a
    local-mcp serve

    # Terminal 2: the on-demand remotely managed Tunnel
    set -a
    . ~/.config/local-mcp/public.env
    set +a
    cloudflared tunnel run --token "$LOCAL_MCP_TUNNEL_TOKEN"

    # Terminal 3+: one local session per project
    cd ~/src/local-mcp
    local-mcp start local-mcp

The repository also includes a `justfile` for these commands. Its development
recipes build and run `target/release/local-mcp` from the current checkout, so
an older globally installed binary cannot silently hide newly added tools.
After installing `just`, the workflow is:

    just build
    just doctor
    just env-check

    # Origin + on-demand Tunnel in one terminal (Ctrl-C stops both)
    just up

    # If a previous run left a child process behind
    just down

    # Or run them separately when independent logs are preferable:
    just serve
    just tunnel

    # Terminal 3+: one session per project
    just start local-mcp
    just start shuttle-rs ~/src/shuttle-rs

The `start` recipe takes the session ID as its first argument and an optional
working directory as its second argument. `doctor`, `serve`, `up`, `start`, and
`mcp` depend on the release build and execute that repository-local binary.
Run one `just start` command per project/session in its own terminal; the origin
and Tunnel recipes are also foreground processes. Use `just install` only when
a globally available `local-mcp` command is required.

The public route is:

    https://temotemcp.example.com/mcp

Cloudflare configuration must provide:

1. A remotely managed Tunnel named local-mcp with
   temotemcp.example.com routed to http://127.0.0.1:8791.
2. A **self-hosted Cloudflare Access application** that directly protects the
   public hostname. Create it under Zero Trust > Access controls >
   Applications > Add application > Self-hosted. Use a name such as
   `localmcp-direct`, the public hostname `temotemcp.example.com`, and leave the
   path empty so the whole host is protected. Do not restrict the application
   to `/mcp`: Managed OAuth discovery is served at the host-root `/.well-known/`
   paths, while the MCP request URL remains `/mcp`.
3. An Allow policy on that application for only the intended email account.
   Do not add a Bypass or Service Auth policy for this public endpoint.
4. Managed OAuth enabled in the self-hosted application's Advanced settings:
   dynamic client registration enabled, a 15-minute access token lifetime, and
   a 14-day grant session duration. The ChatGPT redirect URIs are:
   https://chatgpt.com/connector/oauth/*
   and
   https://chatgpt.com/connector_platform_oauth_redirect.
   The working setup also enabled the localhost and loopback client options
   for local OAuth clients; ChatGPT itself is covered by the explicit HTTPS
   redirect URIs, so disable those options when no local OAuth client needs
   them.

The `AI controls > MCP servers` page serves a different purpose. An entry
created there is a portal registration (often shown as an Access application
with `type: mcp` and a `via_mcp_server_portal` destination); it does not protect
`temotemcp.example.com` when ChatGPT connects directly to that hostname. Such an
entry can remain if the portal is used separately, but it is not part of this
direct Tunnel path.

See the [Cloudflare Managed OAuth documentation](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/managed-oauth/)
and [Cloudflare MCP security guidance](https://developers.cloudflare.com/cloudflare-one/access-controls/ai-controls/secure-mcp-servers/)
for the current Access terminology and API fields.

Managed OAuth is handled by Cloudflare Access. The Rust origin validates the
Cf-Access-Jwt-Assertion signature, issuer, audience, expiry, subject, and
configured email allow list. `LOCAL_MCP_ACCESS_AUDIENCE` must contain the AUD
of the self-hosted application that protects `temotemcp.example.com`; do not
reuse the AUD of the portal-only `type: mcp` entry. If the self-hosted
application is recreated, update `~/.config/local-mcp/public.env` and restart
`local-mcp serve`. The old built-in local OAuth server is not part of the
public path.

In ChatGPT, add a custom MCP app in Developer mode and use the public /mcp
URL. The account's available Plus features are verified at connection time;
the server advertises write and command tools with MCP action annotations so
the client can request confirmation.

Before connecting, check that Access intercepts the request before it reaches
the Rust origin:

    curl -i -X POST https://temotemcp.example.com/mcp \
      -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'
    curl -i https://temotemcp.example.com/.well-known/oauth-authorization-server
    curl -i https://temotemcp.example.com/.well-known/oauth-protected-resource

The unauthenticated MCP request should return `401` with a Cloudflare
`WWW-Authenticate` header, and both discovery endpoints should return `200`
with JSON metadata. A `530` means the on-demand Tunnel or local origin is not
running. A Rust JSON `401` without `WWW-Authenticate`, or a discovery `404`,
means the self-hosted Access application is missing, has the wrong hostname or
path, or is not the application on which Managed OAuth was enabled.

If the origin log says `Cloudflare Access JWT audience is invalid`, copy the
`AUD` value from the self-hosted application (not the portal-only `type: mcp`
entry) into `LOCAL_MCP_ACCESS_AUDIENCE`, then restart `local-mcp serve`. The
Allow policy decides whether Access forwards the request; the Rust origin still
validates the forwarded JWT audience.

If OAuth succeeds but ChatGPT shows no tools, refresh the MCP connection after
restarting `local-mcp serve` (especially after changing the self-hosted
application AUD). The direct connection URL is still exactly
`https://temotemcp.example.com/mcp`; the host root is only used for OAuth
discovery. The public `tools/list` response contains fifteen tools, including the
restricted `git_add`, `git_commit`, `git_fetch`, `git_pull`, and `git_push`
operations; `without_sandbox` is intentionally available only in local stdio
mode. Each public tool must
include a name, display title, description, input schema, and annotations.
Start a new ChatGPT conversation and add the MCP connection from the tools
menu so ChatGPT requests the refreshed tool list. See the [OpenAI
connection guide](https://developers.openai.com/plugins/deploy/connect-chatgpt)
and [MCP troubleshooting guide](https://developers.openai.com/plugins/deploy/troubleshooting)
for the client-side checks.

## Limits and safety boundaries

- Public requests require a valid Cloudflare Access assertion.
- All public tools require an explicit active session_id, except session_list.
- In normal mode, file paths, symlink targets, and command cwd must remain under the
  session's permitted roots.
- In normal mode, sandboxed commands have no network access.
- In `--yolo` mode those two boundaries are disabled: tools may use any host path and
  `execute`/`start_command` run unsandboxed with the local user's inherited environment
  and network access, without local approval prompts.
- Sandboxed commands retain only a minimal environment, including `HOME`, and
  may write temporary files under `/tmp` or `TMPDIR`; do not store secrets
  there.
- Ordinary sandboxed commands cannot write `.git`; use `git_add` and
  `git_commit` for local Git metadata operations.
- Remote Git operations use dedicated approval-gated host tools. `git_pull` is
  fast-forward-only, and `git_push` exposes no force option or arbitrary URL.
- A session has at most four active sandbox jobs.
- Combined stdout/stderr per command is capped at 1 MiB and marked as
  truncated.
- A background sandbox job is cancelled after two hours or when its session
  stops.
- Runtime audit output records tool, session, status, and timing metadata; authenticated email and subject identifiers are intentionally not logged.
  command arguments and output are not persisted as audit logs.
- There is intentionally no secret-file denylist. Do not add broad roots such
  as /home; explicitly permit only the directories needed for the task.

## Local stdio mode

For a local MCP client that starts the process itself:

    local-mcp mcp

This mode is separate from the Cloudflare Access HTTP endpoint. It includes
the local approval UI, the explicitly approved without_sandbox command, and
the restricted Git operations described above.

## Serverless multi-host gateway

The optional [`gateway/`](gateway/) deployment exposes one Cloudflare Workers
MCP endpoint and routes calls through Durable Objects by `session_id`. Mac and
Windows/WSL2 endpoints run `local-mcp gateway-agent`, make outbound-only HTTPS
long polls, and retain the local terminal approval and sandbox boundary. A new
host connection increments its generation so responses from a disconnected or
replaced process cannot complete current requests.

Typical endpoint commands are:

    # Mac
    local-mcp start mac-main
    local-mcp gateway-agent --session-id mac-main

    # Windows interim path, inside WSL2
    local-mcp start windows-wsl2-main
    local-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2

Set `LOCAL_MCP_GATEWAY_URL`, `LOCAL_MCP_GATEWAY_HOST_TOKEN`, and, when the host
route is protected by Cloudflare Access, the service-token client ID and secret.
Deployment, Managed OAuth, Durable Object migration, reconnect behavior, and
secret handling are documented in [`gateway/README.md`](gateway/README.md).
