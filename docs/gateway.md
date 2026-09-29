# Multi-host Cloudflare gateway

[日本語](gateway.ja.md)

Temote Fabric is currently implemented under `gateway/`. This optional Worker exposes one MCP endpoint for a federated set of Temote hosts. A macOS machine, a Linux machine, and a Windows 11 machine running Temote in WSL2 can each run one supervisor plus one host-level gateway agent. The MCP client discovers hosts and selects a host and session without configuring a separate MCP server entry per machine.

Native Windows execution remains a later milestone. Windows 11 federation currently means Temote running inside WSL2.

## Architecture

- `GatewaySession` is used as the Durable Object request/response queue. Host-mode objects are keyed as `host:<host_id>`; legacy per-session objects keep their historical `session_id` key.
- `GatewayRegistry` keeps separately bounded leased registries for federated hosts and legacy per-session agents.
- Worker `/mcp` authenticates MCP clients, exposes host-aware tools, resolves routing, and forwards calls to the selected host.
- `/v1/hosts/*` is the outbound long-poll protocol used by `temote-mcp gateway-agent`.
- The local supervisor remains authoritative for session lifecycle, named-root resolution, sandboxing, and approval. The gateway never receives absolute named-root paths.

A host reconnect increments its generation. Requests and responses from an older generation or process `instance_id` are rejected. Timed-out or disconnected tool calls are not automatically replayed, because routed operations may be non-idempotent.

## Host identity and authentication

`host_id` is a stable, non-secret routing identity such as `mac-main`, `linux-main`, or `win-main`. It is not a credential.

Federated host mode uses a per-host bearer token map stored in the Worker secret `HOST_TOKENS_JSON`. For example, the secret value can be:

```json
{"mac-main":"<random-token-a>","linux-main":"<random-token-b>","win-main":"<random-token-c>"}
```

Each local host receives only its own token through `TEMOTE_MCP_GATEWAY_HOST_TOKEN`. The agent also sends `X-Temote-Host-Id`; the Worker selects the expected credential from `HOST_TOKENS_JSON` and rejects a token that belongs to a different `host_id`. The request body must carry the same `host_id`.

`HOST_TOKEN` remains only for the temporary legacy `gateway-agent --session-id` compatibility path. Cloudflare Access service-token credentials remain separate from both host-token forms.

## Deploy

Run repository `just` commands from the repository root. Run npm and Wrangler commands from `gateway/`, where the pinned package, lockfile, and `wrangler.toml` live; the working directory does not carry between separate code blocks.

1. Use Node.js 22 or newer and install the pinned deploy tooling:

```sh
(cd gateway && npm ci)
```

2. Select a public target, inspect its existing ownership, and configure Cloudflare Access for the whole hostname **before publishing**. Enable Managed OAuth for human MCP clients and a Service Auth policy for host agents. See [Deployment target](#deployment-target). Keep `workers_dev = false`.
3. Set the non-secret `ACCESS_TEAM_DOMAIN`, `ACCESS_AUDIENCE`, `ACCESS_ALLOWED_EMAILS` and `OBSERVATION_OWNER_ID` in the deployment config. Provision `OBSERVATION_DB`, replace its sentinel D1 database ID, and review/apply the additive migrations using the pinned Wrangler from `gateway/`. Keep existing Durable Object class names and bindings when updating a deployed Worker.

```sh
(cd gateway && npx wrangler d1 migrations apply temote-observation --remote)
```

4. Store the per-host token map interactively; preserve `HOST_TOKEN` only if legacy per-session agents still need it. The Access service token and Temote host bearer token are independent credentials.

```sh
(cd gateway && npx wrangler secret put HOST_TOKENS_JSON)
```

5. Generate and check metadata, run tests, then bundle without publishing. After remote configuration and secrets are verified, deploy with the selected target:

```sh
# Repository root
just generate-tools
just check-generated

# gateway/
(cd gateway && npm test)
(cd gateway && npm run deploy:dry-run -- --keep-vars)
(cd gateway && npm run deploy -- --keep-vars)
```

6. Verify health and authenticated MCP reachability using the checks below. A deploy without a target can upload a version without publishing it.

The public MCP URL is `https://<gateway-host>/mcp`. The Worker imports `gateway/contract/routed-tool-metadata.json` generated from Rust; see [generation and stale checks](development.md#connected-runtime-contract-parity). A bundle dry-run proves configuration/build validity, not remote authentication, secret presence or endpoint readiness.

## Deployment target

`workers_dev = false` means a deploy without a route or custom domain does not publish the Worker and can print `No targets deployed`. Treat that output as a failure even when the command exits 0, and choose exactly one target:

Before invoking Wrangler, run the repository-local preflight with the intended target. This command runs from `gateway/`:

```sh
cd gateway
npm run deploy:preflight -- --hostname gateway.example.com --route 'gateway.example.com/*'
```

The preflight reports `target_missing`, `target_mismatch`, or `remote_unknown` as distinct statuses. `remote_unknown` is intentional: this local check does not use Cloudflare credentials and never claims that a deployed route or custom domain exists. Treat every non-zero result as a deployment stop until the target is corrected and the post-deploy read-only verification below succeeds.

| Option | Use when | Target setup | Access | Verification |
| --- | --- | --- | --- | --- |
| A. Custom domain | The Worker should own a dedicated hostname, or no DNS record exists yet. | Declare the hostname as a Worker custom domain, for example `routes = [{ pattern = "<gateway-host>", custom_domain = true }]` in `gateway/wrangler.toml`, or create it in the Cloudflare dashboard. | Protect the whole hostname with a Cloudflare Access application. | Run Wrangler status from `gateway/`; use the Access-authenticated `/healthz` check below. |
| B. Existing DNS + Worker route | A DNS record already exists and must not be deleted. | Pass the exact pattern to the deploy, for example `npx wrangler deploy --keep-vars --routes '<gateway-host>/*'`. | Protect the whole hostname with a Cloudflare Access application. | Same as A; also confirm the route pattern points at `temote-mcp-gateway` in the Cloudflare dashboard. |

Both options follow the same rules:

- Keep `workers_dev = false`. Do not enable the `*.workers.dev` route as a shortcut.
- Do not delete existing DNS records. A Worker route takes precedence for matching requests without removing the record, so the direct origin stays recoverable.
- Use `--keep-vars` when the deployment must preserve dashboard-managed non-secret variables. Worker secrets are unaffected and stay in `wrangler secret`; never write them to `wrangler.toml`, docs, or issue trackers.
- Keep the Access service-token credential and the gateway host token as separate credentials.
- A successful upload is not a successful deployment. Verify the target after every deploy.

### Verify a deployment (read-only)

Run the Wrangler command from `gateway/`. The whole hostname is protected by Access, so send the service-token credentials accepted by its Service Auth policy for the health check. Keep these values in a protected environment or secret store; do not paste them into the command or logs:

```sh
(cd gateway && npx wrangler deployments status --name temote-mcp-gateway)
curl --silent --show-error --fail \
  --header "CF-Access-Client-Id: ${TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID:?set the Access service-token client ID}" \
  --header "CF-Access-Client-Secret: ${TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET:?set the Access service-token client secret}" \
  "https://<gateway-host>/healthz"
```

`/healthz` must return the Temote gateway identity and `readiness=ready` (currently `{"status":"ok","service":"temote-mcp-gateway","readiness":"ready","identity":"temote-mcp-gateway","contractFingerprint":"<sha256>"}`). The Worker does not require a client token for `/healthz`; Cloudflare Access still protects the hostname at the edge. A direct-origin response or a different service identity means the hostname still points at the wrong target. `contractFingerprint` is the SHA-256 digest of the public tool contract; it must equal `gateway/contract/public-tools.fingerprint` from the deployed source revision and the `server_contract_fingerprint` reported by the local/connected server's `session_info`.

For MCP `tools/list` and other `/mcp` requests, use an MCP client authenticated through Access Managed OAuth as a user whose email is in `ACCESS_ALLOWED_EMAILS`. The Worker verifies the Access JWT's signature, audience, issuer, expiry, subject, and allowlisted email. An Access service token is for host-agent Service Auth and the `/healthz` smoke check; its JWT has no user email and an empty subject, so it does not satisfy `/mcp`'s user-identity check. Do not set the local/test `CLIENT_TOKEN` on the production Worker.

### Rollback

Rollback removes only the exact Worker route or custom domain that was added:

1. Remove the exact route pattern or custom-domain binding for `<gateway-host>` in the Cloudflare dashboard or by reverting `gateway/wrangler.toml`, then deploy the previous target set.
2. Leave the DNS record, the Access application, and the Tunnel untouched so the direct origin remains usable.
3. Confirm with `curl -sSf https://<gateway-host>/healthz` and the dashboard that the hostname no longer resolves to the Worker, then restore the intended direct-origin configuration if needed.

Do not remove or rewrite unrelated routes, DNS records, or Access policies as part of a gateway rollback.

## Configure each Temote host

Configure named roots on the machine running the supervisor. Only root names are advertised to the gateway; physical paths remain local.

macOS example:

```sh
export TEMOTE_MCP_ROOTS='{"src":"/Volumes/devstorage/Developer","work":"/Users/me/work"}'
export TEMOTE_MCP_GATEWAY_HOST_ID=mac-main
export TEMOTE_MCP_GATEWAY_URL=https://<gateway-host>
export TEMOTE_MCP_GATEWAY_HOST_TOKEN='<token assigned to mac-main>'
export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID='<Access service-token ID>'
export TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET='<Access service-token secret>'

temote-mcp supervisor
temote-mcp gateway-agent --host-id mac-main
```

Linux uses the same model with Linux paths. On Windows 11, run the supervisor and agent inside WSL2 and use WSL paths such as `/mnt/d/Developer`:

```sh
export TEMOTE_MCP_ROOTS='{"src":"/mnt/d/Developer"}'
temote-mcp supervisor
temote-mcp gateway-agent --host-id win-main --platform wsl2
```

`--platform auto` detects macOS, Linux, and WSL2. When `TEMOTE_MCP_GATEWAY_HOST_ID` is configured, `temote-mcp doctor` reports staged gateway readiness: each `local_config` item (host ID, gateway URL origin, host token presence, Access service-token pair) and the `local_supervisor` control protocol are separate results. With the network-enabled build it also performs a read-only `/healthz` identity check and authenticated `/v1/hosts/status` probe. These classify the remote endpoint, Access authorization, and this host's active lease. Doctor also reports `session_availability` from the local supervisor's read-only session inventory as `listed_sessions`/`active_sessions` counts; this reuses the supervisor control protocol, never dispatches an MCP tool, and never mutates a session or lease. A confirmed inventory whose live (`active`/`starting`) session count is zero is reported as `failed`, not `ready`; an inventory that cannot be enumerated is `unavailable`. When a local host-level `gateway-agent` generation is recorded, doctor compares the authenticated gateway `generation` against it and reports `generation_replaced` when the remote generation is newer, so a superseded local agent is not mistaken for a healthy one. The host-level `gateway-agent` separately reports a bounded, non-secret `session_availability` value (`ready`, `session_unavailable`, or `unavailable`) on each poll; the authenticated `/v1/hosts/status` response returns the latest reported value, and an unreported value stays `not_checked` instead of being treated as `ready`. That remote value is derived read-only from the supervisor inventory and carries no session ID, path, or credential. Doctor never prints root paths or token values.

## MCP workflow

The gateway adds host discovery and host-aware lifecycle routing:

```text
host_list()
host_info(host_id="linux-main")
session_list(host_id="linux-main")
session_start(host_id="linux-main", path="src/project-a", session_id="project-a")
session_info(host_id="linux-main", session_id="project-a")
```

`session_start` accepts only a named-root-relative logical path. The host-side public supervisor always creates a normal sandboxed session; a remote client cannot request `--yolo`.

The same human-friendly session ID may exist on multiple hosts:

```text
mac-main / srmj
linux-main / srmj
```

Use explicit `host_id` for deterministic routing. For backwards compatibility, an omitted `host_id` may resolve an existing `session_id` only when exactly one currently discoverable owner exists. If two hosts own the same ID, or ownership cannot be determined safely because a leased host cannot be queried, the gateway fails closed instead of choosing a host.

`session_stop` and `session_restart` apply only to active sessions owned by the host's public supervisor. Separately started local CLI/yolo sessions remain local-only: public session-bound tools reject yolo targets rather than inheriting their unrestricted semantics.

## Lease and failure behavior

- Each poll refreshes a 90-second host lease and waits up to 20 seconds for work.
- Gateway dispatch waits up to 35 seconds for the endpoint response.
- `host_list` includes only registry entries whose corresponding host Durable Object still reports an active lease.
- A reconnect replaces the old generation and fences the previous agent instance.
- Disconnect, lease expiry, timeout, or Worker replacement never causes automatic replay of an ambiguous mutating tool call.
- Aggregate unqualified session discovery fails closed if a currently leased host cannot be queried.
- Local sandbox and approval policy are always enforced on the execution host.

## Migration from per-session agents

The existing command remains temporarily supported:

```sh
temote-mcp gateway-agent --session-id old-session
```

It continues to use `HOST_TOKEN`, a Durable Object keyed directly by `session_id`, and the old session-oriented lease flow. New deployments should use one `--host-id` agent per supervisor instead.

Host mode requires the current supervisor control protocol. This change advances that protocol, so upgrade/restart the Temote supervisor before starting a host-level gateway agent. Mixed versions fail clearly instead of silently omitting the new lifecycle safety fields.

During migration, legacy sessions and host-level agents may coexist. Unqualified session routing checks both populations and fails closed on collisions.

## Dashboard

The same Worker serves the Access-protected read-only dashboard at `/dash/`. See [dashboard operation and verification](fabric-dashboard.md) for authority, freshness, Host prerequisites, and deployment acceptance.

## Development

For local Worker development, copy `gateway/.dev.vars.example` to `gateway/.dev.vars`. Never commit `.dev.vars`, Worker secrets, Access service-token secrets, host bearer tokens, or endpoint environment files.

The routed tool schemas and MCP protocol versions are checked against a Rust-generated contract snapshot in both Rust and Node tests. `serverInfo.version` is the Cloudflare deployment revision from the `GATEWAY_DEPLOYMENT` version-metadata binding, not the Temote CLI CalVer.
