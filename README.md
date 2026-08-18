# temote-mcp

[日本語](README.ja.md)

This repository is derived from [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp), with a focus on secure remote access, gateway operation, and MCP bridging. GitHub does not record this repository as a fork, so the relationship is stated here.

The name also draws on [@mr_konn's proposal of 「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46), coined in Japanese as the opposite of “remote.” The original post does not give a Latin spelling; this repository uses **Temote**, including the example hostname `temotemcp.example.com`. We credit both the upstream codebase and this naming idea as part of the project's origin.

temote-mcp exposes local files and sandboxed commands as MCP tools. It does
not provide web search or a general network-request tool. Sandboxed commands
run with network access disabled. Linux uses the pinned Codex sandbox stack;
macOS uses temote-mcp's native Seatbelt backend.

The public HTTP endpoint is designed for an on-demand Cloudflare Tunnel:

    MCP client
        | OAuth
        v
    Cloudflare Access -- Cloudflare Tunnel -- 127.0.0.1:8791
                                                   |
                                                   v
                                            temote-mcp serve

## Build

    cargo build --release
    cargo install --path . --locked

The default build includes the public HTTP and gateway-agent commands. For a
local-only binary containing `doctor`, `start`, and `mcp` without temote-mcp's
direct HTTP/JWT client dependencies, disable the default `network` feature:

    cargo build --release --no-default-features --locked

On Linux, install both temote-mcp and the sibling codex-linux-sandbox binary
in the same directory, and make sure bwrap is available in PATH. macOS uses
the system Seatbelt sandbox. Native Windows is not supported.

Keep `--locked` when installing. The Linux Codex sandbox dependencies are
pinned to a Git revision, and resolving their transitive pre-release
dependencies without the committed lockfile can select incompatible `rama` or
`starlark` versions. macOS does not resolve the Codex sandbox crates.

Before starting a session or exposing the HTTP endpoint, run the host diagnostics:

    temote-mcp doctor

On Linux, `doctor` checks the installed `codex-linux-sandbox` helper,
`bubblewrap`, user-namespace settings, the isolated network namespace, a real
temote-mcp sandbox command, and the runtime environment used by shell commands.
A failed `network namespace` check means the host cannot create the bwrap
network namespace; fix the displayed host policy hint before starting the HTTP
server. The command exits non-zero when a required check fails.

If an MCP client reports that `/.bash_profile` or `/tmp` cannot be found or written,
update the installed binary with `just install`, run `temote-mcp doctor`, and
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

    cd ~/src/temote-mcp
    temote-mcp start temote-mcp

    cd ~/src/shuttle-rs
    temote-mcp start shuttle-rs

For an explicitly unrestricted session, add `--yolo`:

    temote-mcp start temote-mcp --yolo

YOLO mode is intentionally dangerous: temote-mcp approval prompts are skipped, path
roots are not enforced, and command tools run directly on the host with the
filesystem, environment, process, and network permissions of the user running
`temote-mcp`. The mode is stored in the active session metadata so local stdio, HTTP,
and gateway requests all observe the same setting. This setting only controls
temote-mcp boundaries; any confirmation or authorization enforced by an MCP client is
independent. `/permission ask` restores the normal restricted mode and
`/permission yolo` enables it again while the session runs.

Every tool call except `session_list` requires a `session_id`. `session_list`
discovers active sessions, and `session_info` shows a session's working
directory and sandbox roots. Session IDs may contain ASCII letters, digits,
`-`, `_`, and `.` only. A duplicate active ID is rejected.

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

Use the dedicated `git_add` and `git_commit` tools to stage files and create
local commits. Use `git_fetch`, `git_pull`, and `git_push` for remote
synchronization. The remote tools execute on the host only after approval
(unless the session is in yolo mode), accept configured remote names rather
than arbitrary URLs or refspecs, disable hooks, reject force-push options, and
make `git_pull` fast-forward-only. Ordinary `execute` commands keep `.git`
metadata read-only and network access disabled.

## Public HTTP endpoint

Copy .env.example to a file outside the repository, such as
~/.config/temote-mcp/public.env, and set the Cloudflare Access values. Keep
the file mode 0600; the Tunnel token is a credential.

    install -d -m 700 ~/.config/temote-mcp
    cp .env.example ~/.config/temote-mcp/public.env
    chmod 600 ~/.config/temote-mcp/public.env
    vi ~/.config/temote-mcp/public.env

Use separate terminals when the service is needed:

    # Terminal 1: the local origin
    set -a
    . ~/.config/temote-mcp/public.env
    set +a
    temote-mcp serve

    # Terminal 2: the on-demand remotely managed Tunnel
    set -a
    . ~/.config/temote-mcp/public.env
    set +a
    cloudflared tunnel run --token "$TEMOTE_MCP_TUNNEL_TOKEN"

    # Terminal 3+: one local session per project
    cd ~/src/temote-mcp
    temote-mcp start temote-mcp

The repository also includes a `justfile` for these commands. Its development
recipes build and run `target/release/temote-mcp` from the current checkout, so
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
    just start temote-mcp
    just start shuttle-rs ~/src/shuttle-rs

The `start` recipe takes the session ID as its first argument and an optional
working directory as its second argument. `doctor`, `serve`, `up`, `start`, and
`mcp` depend on the release build and execute that repository-local binary.
Run one `just start` command per project/session in its own terminal; the origin
and Tunnel recipes are also foreground processes. Use `just install` only when
a globally available `temote-mcp` command is required.

The public route is:

    https://temotemcp.example.com/mcp

Cloudflare configuration must provide:

1. A remotely managed Tunnel named temote-mcp with
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
4. Managed OAuth enabled in the self-hosted application's Advanced settings.
   Enable dynamic client registration and choose access-token and grant-session
   lifetimes appropriate for the deployment. Configure redirect URIs and local
   or loopback client options only when required by the intended OAuth client.

The `AI controls > MCP servers` page serves a different purpose. An entry
created there is a portal registration (often shown as an Access application
with `type: mcp` and a `via_mcp_server_portal` destination); it does not protect
`temotemcp.example.com` itself. Such an entry can remain if the portal is used
separately, but it is not part of this direct Tunnel path.

See the [Cloudflare Managed OAuth documentation](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/managed-oauth/)
and [Cloudflare MCP security guidance](https://developers.cloudflare.com/cloudflare-one/access-controls/ai-controls/secure-mcp-servers/)
for the current Access terminology and API fields.

Managed OAuth is handled by Cloudflare Access. The Rust origin validates the
Cf-Access-Jwt-Assertion signature, issuer, audience, expiry, subject, and
configured email allow list. `TEMOTE_MCP_ACCESS_AUDIENCE` must contain the AUD
of the self-hosted application that protects `temotemcp.example.com`; do not
reuse the AUD of the portal-only `type: mcp` entry. If the self-hosted
application is recreated, update `~/.config/temote-mcp/public.env` and restart
`temote-mcp serve`. The old built-in local OAuth server is not part of the
public path.

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
entry) into `TEMOTE_MCP_ACCESS_AUDIENCE`, then restart `temote-mcp serve`. The
Allow policy decides whether Access forwards the request; the Rust origin still
validates the forwarded JWT audience.

If OAuth succeeds but an MCP client shows no tools, refresh the connection after
restarting `temote-mcp serve` (especially after changing the self-hosted
application AUD). The direct connection URL is still exactly
`https://temotemcp.example.com/mcp`; the host root is only used for OAuth
discovery. The public `tools/list` response includes the restricted `git_add`,
`git_commit`, `git_fetch`, `git_pull`, and `git_push` operations plus the
configured child-MCP bridges; `without_sandbox` is intentionally available only
in local stdio mode. Each public tool includes a name, display title,
description, input schema, and annotations.

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
- Runtime audit output records tool, session, status, and timing metadata.
  Authenticated email and subject identifiers, command arguments, and command
  output are not persisted in audit logs.
- There is intentionally no secret-file denylist. Do not add broad roots such
  as /home; explicitly permit only the directories needed for the task.

## Local stdio mode

For a Temote MCP client that starts the process itself:

    temote-mcp mcp

This mode is separate from the Cloudflare Access HTTP endpoint. It includes
the local approval UI, the explicitly approved without_sandbox command, and
the restricted Git operations described above.

## 1Password Environments MCP

temote-mcp can bridge the official local 1Password Environments MCP server so a
remote client can use the same 1Password Developer workflow without exposing a
new inbound port on the host. Enable **MCP Server** in 1Password Labs/Developer
settings first. temote-mcp uses the bundled `1password-mcp` binary at
`/Applications/1Password.app/Contents/MacOS/1password-mcp` on macOS or
`/opt/1Password/1password-mcp` on Linux. Set `TEMOTE_MCP_ONEPASSWORD_MCP` to an
absolute path only for a non-standard installation.

The public and Temote MCP endpoints expose three bridge tools:

- `onepassword_mcp_discover` lists the official child server's current resources
  and tool schemas. Use it before calling child tools.
- `onepassword_mcp_read_resource` reads only resources advertised by that child
  server, including its getting-started and Environments guides.
- `onepassword_mcp_call` forwards a named child tool call over a persistent stdio
  connection. Calls whose child tool is not marked read-only use temote-mcp's
  normal approval UI unless the selected session is in yolo mode.

For normal sessions, `create_local_env_file.mountPath` is additionally constrained
to the session's permitted filesystem roots. Approval summaries include argument
keys only and never persist argument values. Secret handling remains owned by the
official 1Password MCP server; temote-mcp does not add a secret-reading API or
translate 1Password data into environment variables itself.

### Service-account mode

For unattended secret injection, start the temote-mcp session with a 1Password
service-account token:

    OP_SERVICE_ACCOUNT_TOKEN='<service-account-token>' temote-mcp start my-project

The token is captured by the `temote-mcp start` process and forwarded only to
transient `op` subprocesses. It is not written to session JSON metadata, returned
by any MCP tool, or required by the gateway agent. Keep `temote-mcp start` running for the lifetime of the session.

Two additional tools are available:

- `onepassword_service_account_status` checks whether the session has a token and
  whether `op whoami` accepts it, without returning the token.
- `onepassword_service_account_run` executes a command through `op run`. Provide
  secret references either as `environment` entries such as
  `DATABASE_URL=op://vault/item/field` or through checked-in `.env` files using
  `env_files`. Plaintext values in `environment` are rejected. 1Password CLI's
  output masking remains enabled and `OP_SERVICE_ACCOUNT_TOKEN` is removed from
  the target command's direct environment.

Normal temote-mcp sessions still require host approval before a service-account
command runs. A session started with `--yolo` skips that temote-mcp approval, so
for fully unattended operation use:

    OP_SERVICE_ACCOUNT_TOKEN='<service-account-token>' temote-mcp start my-project --yolo

`env_files` and `cwd` remain restricted to the session's permitted filesystem
roots in normal mode. The service-account token should be scoped to only the
vaults required by the automation.

## kintone MCP Server

temote-mcp can also bridge the official [`@kintone/mcp-server`](https://github.com/kintone/mcp-server).
Install its CLI on the host first; the upstream package currently requires Node.js 22 or newer:

    npm install -g @kintone/mcp-server

Start the temote-mcp session with the kintone settings on the **`temote-mcp start`
process**, not on `temote-mcp serve` or `gateway-agent`. The session process keeps
the credential values in memory and does not write them to session metadata or
return them through MCP:

    KINTONE_BASE_URL='https://example.cybozu.com' \
    KINTONE_API_TOKEN='<api-token>' \
    temote-mcp start my-project

Username/password authentication is also supported:

    KINTONE_BASE_URL='https://example.cybozu.com' \
    KINTONE_USERNAME='<username>' \
    KINTONE_PASSWORD='<password>' \
    temote-mcp start my-project

The bridge passes the official configuration variables supported by the upstream
server: `KINTONE_BASE_URL`, username/password or `KINTONE_API_TOKEN`, optional
Basic-auth credentials, PFX client-certificate settings, `HTTPS_PROXY`/`https_proxy`, and
`KINTONE_ATTACHMENTS_DIR`. `KINTONE_PFX_FILE_PATH` and
`KINTONE_ATTACHMENTS_DIR` must remain inside the selected session's permitted
filesystem roots in normal mode. Set `TEMOTE_MCP_KINTONE_MCP` to an absolute
path when `kintone-mcp-server` is not on `PATH`.

Three public/local tools are exposed:

- `kintone_mcp_status` reports whether the executable and required configuration
  are present without returning credential values or the tenant URL.
- `kintone_mcp_discover` starts the child server on demand and lists its current
  tool schemas.
- `kintone_mcp_call` forwards one named child tool call over the session-local
  stdio connection. Because the current upstream server does not annotate tools
  as read-only versus mutating, every forwarded call is approval-gated in normal
  mode. A session started with `--yolo` skips that local approval.

The child process receives only the small runtime environment needed to launch
Node plus the allow-listed kintone variables. Other credentials present in the
`temote-mcp start` environment are not inherited by the kintone child. To source
kintone credentials from 1Password, run `temote-mcp start` itself through
`op run`; the resolved values are then captured by the session process without
being placed in temote-mcp's session JSON.

## Serverless multi-host gateway

The optional [`gateway/`](gateway/) deployment exposes one Cloudflare Workers
MCP endpoint and routes calls through Durable Objects by `session_id`. Mac and
Windows/WSL2 endpoints run `temote-mcp gateway-agent`, make outbound-only HTTPS
long polls, and retain the local terminal approval and sandbox boundary. A new
host connection increments its generation so responses from a disconnected or
replaced process cannot complete current requests.

Typical endpoint commands are:

    # Mac
    temote-mcp start mac-main
    temote-mcp gateway-agent --session-id mac-main

    # Windows interim path, inside WSL2
    temote-mcp start windows-wsl2-main
    temote-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2

Set `TEMOTE_MCP_GATEWAY_URL`, `TEMOTE_MCP_GATEWAY_HOST_TOKEN`, and, when the host
route is protected by Cloudflare Access, the service-token client ID and secret.
Deployment, Managed OAuth, Durable Object migration, reconnect behavior, and
secret handling are documented in [`gateway/README.md`](gateway/README.md).
