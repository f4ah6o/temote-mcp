# Remote connection profiles

[日本語](public-http.ja.md)

Temote MCP supports three production connection profiles. Cloudflare and Tailscale provide general-purpose public HTTPS endpoints; OpenAI Secure MCP Tunnel is an outbound-only private connection for supported OpenAI products.

| Profile | Connection | Authentication / trust boundary |
| --- | --- | --- |
| `cloudflare` | Cloudflare Tunnel public HTTPS | Cloudflare Access Managed OAuth |
| `tailscale` | Tailscale Funnel public HTTPS | Temote local OAuth |
| `openai` | OpenAI Secure MCP Tunnel | OpenAI tunnel connection + Temote local sandbox/approval |

Omitting `--profile` keeps the existing `cloudflare` behavior for compatibility. All three profiles terminate at the same provider-neutral MCP core. Remote access never exposes `without_sandbox`; managed sessions keep the same named-root, sandbox, and local runtime-approval rules regardless of profile.

## Cloudflare profile

```text
MCP client
    | Managed OAuth
    v
Cloudflare Access -- Cloudflare Tunnel -- 127.0.0.1:8791
                                               |
                                               v
                               temote-mcp up --profile cloudflare
```

Keep Cloudflare deployment settings outside the repository and mode `0600`:

```sh
install -d -m 700 ~/.config/temote-mcp
cp .env.example ~/.config/temote-mcp/public.env
chmod 600 ~/.config/temote-mcp/public.env
```

Required values are:

- `TEMOTE_MCP_PUBLIC_URL`
- `TEMOTE_MCP_ACCESS_TEAM_DOMAIN`
- `TEMOTE_MCP_ACCESS_AUDIENCE`
- `TEMOTE_MCP_ACCESS_ALLOWED_EMAILS`
- `~/.config/temote-mcp/tunnel-token` (mode `0600`; override with `TUNNEL_TOKEN_FILE`)

The Cloudflare profile loads `~/.config/temote-mcp/public.env` (or `TEMOTE_MCP_ENV_FILE`) and keeps the existing Access defense in depth: the origin validates the forwarded `Cf-Access-Jwt-Assertion` signature, issuer, audience, expiry, subject, and configured email allow list.

Run the lifecycle supervisor first, then start the origin and Tunnel separately:

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor
# another terminal/service
temote-mcp up --profile cloudflare
```

`temote-mcp up` without `--profile` is equivalent. `up/serve` requires the local supervisor control socket and never owns session runtimes. `temote-mcp down` stops the HTTP origin and its managed Tunnel child only; the lifecycle supervisor and sessions remain alive.

To run the origin without starting `cloudflared`:

```sh
set -a
. ~/.config/temote-mcp/public.env
set +a
temote-mcp serve --profile cloudflare
```

For a route such as `https://temotemcp.example.com/mcp`:

1. Route a remotely managed Tunnel hostname to `http://127.0.0.1:8791`.
2. Protect the entire hostname with a self-hosted Cloudflare Access application. Managed OAuth discovery uses host-root `/.well-known/` paths, so do not scope Access only to `/mcp`.
3. Add an Allow policy for the intended identities; do not use Bypass for the public MCP route.
4. Enable Managed OAuth for the intended clients.
5. Put the self-hosted application's `AUD` in `TEMOTE_MCP_ACCESS_AUDIENCE`.

A Cloudflare `AI controls > MCP servers` portal registration is separate from the self-hosted Access application protecting the hostname.

## Tailscale profile

```text
MCP client
    | Authorization Code + PKCE S256
    v
Temote local OAuth -- Tailscale Funnel -- 127.0.0.1:8791
                                               |
                                               v
                                temote-mcp up --profile tailscale
```

The Tailscale profile requires a connected Tailscale CLI/node with Funnel available. It does **not** require a Cloudflare account, Tunnel token, Access application, Access audience, or email allow list.

Start it with:

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor
# another terminal/service
temote-mcp doctor --profile tailscale
temote-mcp up --profile tailscale
```

When no public URL is explicitly supplied, Temote derives the canonical `*.ts.net` hostname from `tailscale status --json` and selects the first free supported Funnel HTTPS port in the order `443`, `8443`, `10000`. The managed Funnel proxies only to the local `127.0.0.1` origin. Existing Funnel configuration is never replaced; if all three supported ports are occupied, startup fails closed.

The Tailscale daemon itself is never stopped. `temote-mcp down` terminates only the Temote HTTP origin and its direct `tailscale funnel` child; it does not stop the lifecycle supervisor or sessions.

`temote-mcp serve --profile tailscale --public-url <https-origin>` runs the local OAuth/MCP origin without starting Funnel. This is useful when ingress is managed separately. For `temote-mcp up --profile tailscale`, an explicit public URL must use the current node's `*.ts.net` hostname and one of the supported Funnel HTTPS ports (`443`, `8443`, `10000`).

The Tailscale profile does not load the Cloudflare-oriented `public.env`. A shell-level `TEMOTE_MCP_PUBLIC_URL` is only a fallback when the Tailscale node hostname cannot be derived; `--public-url` is the explicit override for `serve`.

### Temote local OAuth

The local authorization server exposes:

```text
/.well-known/oauth-protected-resource
/.well-known/oauth-authorization-server
/register
/authorize
/token
/mcp
```

The flow uses Authorization Code with mandatory PKCE `S256`. Authorization codes are short-lived, single-use, and bound to the exact `client_id`, redirect URI, and MCP resource. Access tokens are opaque, short-lived bearer tokens bound to the exact `/mcp` resource. Code/token values are not included in normal logs or approval summaries.

Client discovery supports current Client ID Metadata Documents and keeps Dynamic Client Registration at `/register` for client compatibility. Metadata-document fetches are limited to HTTPS port 443 public DNS destinations, do not follow redirects, reject private/loopback/special-use addresses, and enforce a bounded response size.

The first authorization decision is local-owner approval through `temote-mcp session console`. `serve/up` proxies that request over the owner-only supervisor control socket and does not expose approval over HTTP. The console shows the client, redirect URI, resource, and scope. This OAuth approval is separate from later host/network-sensitive tool approvals. Authentication never creates a yolo session.

Registrations, pending authorization codes, and access tokens are bounded process-local state. Restarting Temote invalidates that local OAuth state; no password database, email database, or persistent bearer-token file is required.

## OpenAI Secure MCP Tunnel profile

The `openai` profile is for supported OpenAI products that can reach a private/local MCP server through OpenAI Secure MCP Tunnel. It does not create a public Internet endpoint and does not use `TEMOTE_MCP_PUBLIC_URL`, Cloudflare, or Tailscale.

Temote can create the tunnel record through the OpenAI Tunnel Management API. For interactive use, `openai setup` asks for the Admin API key on the controlling terminal with echo disabled when `OPENAI_ADMIN_KEY` is absent:

```sh
temote-mcp openai setup --workspace-id '<CHATGPT_WORKSPACE_ID>'
```

`openai setup` sends `POST https://api.openai.com/v1/tunnels` by default, requires at least one `--workspace-id` or `--organization-id`, and stores only the returned `CONTROL_PLANE_TUNNEL_ID` in `~/.config/temote-mcp/openai.env` with private permissions. API keys are deliberately not written to that file. Existing saved tunnel state is not replaced unless `--force` is supplied. The official `CONTROL_PLANE_BASE_URL` override is accepted only when it is an HTTPS origin without credentials or a path. `OPENAI_ADMIN_KEY` remains supported for non-interactive setup.

The Runtime API key is a separate credential and is not created by this command. Create a Restricted Runtime API key with **Tunnels Read + Use**. `temote-mcp up --profile openai` uses `CONTROL_PLANE_API_KEY` or the official `OPENAI_API_KEY` fallback when present; otherwise it asks for the Runtime API key on the controlling terminal with echo disabled. The prompted value is not written to argv, shell environment, or Temote config: it is injected only into the Temote-owned `tunnel-client` child environment and the source buffer is zeroized after spawn. `OPENAI_ADMIN_KEY` is explicitly removed from that runtime child.

Temote integrates with the official `openai/tunnel-client`. The runtime requires:

- `tunnel-client` on `PATH`, or `TUNNEL_CLIENT_BIN` pointing to the binary
- `CONTROL_PLANE_TUNNEL_ID` from the environment or the saved `openai.env` bootstrap state
- a Restricted Runtime API key, supplied by hidden prompt for interactive `up` or by environment for non-interactive operation

`doctor --profile openai` is intentionally non-interactive. When validating control-plane access it requires `CONTROL_PLANE_API_KEY` or the official `OPENAI_API_KEY` fallback in the environment. A normal interactive start does not.

```sh
temote-mcp up --profile openai
```

`temote-mcp up --profile openai` binds the local MCP origin only to loopback and starts a Temote-owned child equivalent to:

```text
tunnel-client run \
  --control-plane.tunnel-id <configured tunnel> \
  --mcp.server-url http://127.0.0.1:8791/mcp
```

The exact local port follows `--addr`; non-loopback bind addresses are rejected. `temote-mcp down` stops the Temote HTTP origin and its direct `tunnel-client` child only; the lifecycle supervisor and sessions remain alive. It does not create a public listener, public OAuth server, Cloudflare Tunnel, or Tailscale Funnel.

The tunnel transport is not treated as a reason to enable yolo mode. Remote tool calls still enter the same managed-session, named-root, sandbox, and host/network-sensitive approval boundaries. OpenAI tunnel identity fields that are not supplied by the tunnel are not invented by Temote.

Official references: [OpenAI tunnel-client](https://github.com/openai/tunnel-client) and [ChatGPT developer mode / MCP connectors](https://help.openai.com/en/articles/12584461-developer-mode-and-full-mcp-connectors-in-chatgpt).

## Diagnostics

Profile-aware checks are explicit:

```sh
temote-mcp doctor --profile cloudflare
temote-mcp doctor --profile tailscale
temote-mcp doctor --profile openai
```

Cloudflare checks cover `cloudflared`, the private Tunnel token file, and Access configuration. Add `--cloudflare` to query the configured Tunnel through the Cloudflare API; the additional diagnostic environment variables are documented in [development diagnostics](development.md).

Tailscale checks cover the CLI/node connection, canonical `*.ts.net` endpoint, current Funnel ownership across HTTPS ports `443`/`8443`/`10000`, the next port Temote can safely own, and local OAuth state. Cloudflare credentials are not checked for the Tailscale profile.

OpenAI checks cover the official `tunnel-client`, tunnel ID/runtime-key configuration, control-plane access when credentials are present, and the loopback-only local bind policy. OpenAI diagnostics do not require Cloudflare or Tailscale configuration.

Bare `temote-mcp doctor` preserves the previous local behavior and only performs Cloudflare Tunnel checks when that configuration is present or explicitly requested.

## MCP protocol compatibility

The HTTP MCP origin supports MCP `2026-07-28` and the existing 2025-era handshake. Modern requests use `server/discover`, per-request `_meta`, and the `MCP-Protocol-Version` / `Mcp-Method` / `Mcp-Name` HTTP headers. Legacy clients continue to use `initialize`; no `Mcp-Session-Id` is created for modern requests.

For the Tailscale profile, an unauthenticated `/mcp` request returns `401` with a Bearer `WWW-Authenticate` challenge containing the protected-resource metadata URL. For the Cloudflare profile, Cloudflare Access remains the external Managed OAuth boundary and the Rust origin still rejects an invalid or missing Access assertion.

## Remote tool and managed-session boundary

With `TEMOTE_MCP_ROOTS` configured, authenticated HTTP clients can use `session_start` and `session_stop`. `session_start` accepts only logical named-root-relative paths and has no yolo option. Absolute paths, unknown roots, traversal, symlink escape, and roots-unset fallback are rejected. `session_stop` is limited to sessions marked HTTP-owned by the lifecycle supervisor; local CLI/yolo sessions cannot be stopped remotely.

Remote profiles do not expose `without_sandbox`. Normal sessions keep filesystem containment and a network-disabled sandbox, while host/network-sensitive operations still require local approval. Public HTTP authentication is therefore an identity boundary, not a replacement for Temote's session/sandbox/approval boundaries.
