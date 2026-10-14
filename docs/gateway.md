# Temote Fabric on Cloudflare

[日本語](gateway.ja.md)

Temote Fabric is currently implemented under `fabric/`. This optional Worker exposes one MCP endpoint for a federated set of Temote hosts. A macOS machine, a Linux machine, and a Windows 11 machine running Temote in WSL2 can each run one supervisor plus one host-level gateway agent. The MCP client discovers hosts and selects a host and session without configuring a separate MCP server entry per machine.

Native Windows execution remains a later milestone. Windows 11 federation currently means Temote running inside WSL2.

## Architecture

- `GatewaySession` is used as the Durable Object request/response queue. Host-mode objects are keyed as `host:<host_id>`; legacy per-session objects keep their historical `session_id` key.
- `GatewayRegistry` keeps separately bounded leased registries for federated hosts and legacy per-session agents.
- Worker `/mcp` authenticates MCP clients, exposes host-aware tools, resolves routing, and forwards calls to the selected host.
- `/v1/hosts/*` is the outbound long-poll protocol used by `temote-mcp gateway-agent`.
- The local supervisor remains authoritative for session lifecycle, named-root resolution, sandboxing, and approval. The gateway never receives absolute named-root paths.
- Delegation observations first enter an owner-only local journal. The host agent replicates eligible records to D1 through its authenticated host channel; D1 is a sanitized replica, not execution authority.
- Queue messages wake the Memory Worker after committed D1 ingest. D1 retains pending work so the scheduled sweep can recover missed queue sends and lagging projections.
- Knowledge is a derived, provenance-bearing projection. It does not authorize or start execution.

A host reconnect increments its generation. Requests and responses from an older generation or process `instance_id` are rejected. Timed-out or disconnected tool calls are not automatically replayed, because routed operations may be non-idempotent.

New host agents negotiate up to eight concurrent RPCs; gateways that do not advertise capacity keep the original serial protocol. When all slots are occupied, the agent sends a lease heartbeat without accepting another request. Accepted RPCs remain owned across a transport reconnect, and a failed response upload retries only the same response envelope. Generation replacement still fences old responses; no RPC is automatically replayed.

## Host identity and authentication

`host_id` is a stable, non-secret routing identity such as `mac-main`, `linux-main`, or `win-main`. It is not a credential.

Federated host mode uses a per-host bearer token map stored in the Worker secret `HOST_TOKENS_JSON`. For example, the secret value can be:

```json
{"mac-main":"<random-token-a>","linux-main":"<random-token-b>","win-main":"<random-token-c>"}
```

Each local host receives only its own token through `TEMOTE_MCP_GATEWAY_HOST_TOKEN`. The agent also sends `X-Temote-Host-Id`; the Worker selects the expected credential from `HOST_TOKENS_JSON` and rejects a token that belongs to a different `host_id`. The request body must carry the same `host_id`.

`HOST_TOKEN` remains only for the temporary legacy `gateway-agent --session-id` compatibility path. Cloudflare Access service-token credentials remain separate from both host-token forms.

## Deploy

Use the pinned `cf` CLI and `cloudflare.config.ts` for the canonical deployment.
The ignored `TEMOTE_DEPLOYMENT_CONFIG` profile selects the Worker, database,
domain and non-secret Access policy. Follow the staged [naming migration](naming-migration.md)
when upgrading an existing Gateway; retain both namespace IDs, the D1 ID and
the source credential authority. Capture a D1 Time Travel bookmark before
applying migrations, and verify foreign-key integrity afterward.

```sh
# Repository root
just generate-tools
just check-generated
(cd fabric && npm test)
# From the Fabric source directory, with a reviewed non-secret profile:
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf build
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf deploy --prebuilt --dry-run
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf deploy --prebuilt
```

Existing secrets are inherited by name. New secrets require authorized runtime
injection; do not copy secret values into a profile or command. Events and
memory capabilities remain disabled until their dependencies are configured.
An upload or dry-run does not prove domain routing, Access authentication or
Host/session availability. Verify those after each applicable stage.

### Wrangler compatibility deployment

The following procedure retains the legacy Wrangler configuration for existing
operators. Its command flags apply to Wrangler, while the package's deployment
scripts now invoke `cf`.

Run repository `just` commands from the repository root. Run npm and Wrangler commands from `fabric/`, where the pinned package, lockfile, and `wrangler.toml` live; the working directory does not carry between separate code blocks.

1. Use Node.js 22 or newer and install the pinned deploy tooling:

```sh
(cd fabric && npm ci)
```

2. Select a public target, inspect its existing ownership, and configure Cloudflare Access for the whole hostname **before publishing**. Enable Managed OAuth for human MCP clients and a Service Auth policy for host agents. See [Deployment target](#deployment-target). Keep `workers_dev = false`.
3. Set the non-secret `ACCESS_TEAM_DOMAIN`, `ACCESS_AUDIENCE`, `ACCESS_ALLOWED_EMAILS` and `OBSERVATION_OWNER_ID` in the deployment config. Provision `OBSERVATION_DB`, replace its sentinel D1 database ID, inspect pending migrations, and apply them using the pinned Wrangler from `fabric/`. Migration `0003_memory_worker.sql` rebuilds the knowledge tables while copying existing knowledge, support, and supersession rows; review it against the target database before applying. Keep existing Durable Object class names and bindings when updating a deployed Worker.

Apply the complete ordered D1 migration set, including `0006_browser_host_enrollment.sql`, before deploying a Worker build that contains browser enrollment. Legacy federated-host authorization now checks the grant table to reserve browser host IDs; if the table is missing or D1 cannot answer that check, the Worker deliberately rejects the host request. Do not deploy the Worker ahead of the migration, and do not treat a missing-table 401 as a host-token configuration problem.

```sh
(cd fabric && npx wrangler d1 migrations list temote-observation --remote)
(cd fabric && npx wrangler d1 migrations apply temote-observation --remote)
```

4. Store the per-host token map interactively; preserve `HOST_TOKEN` only if legacy per-session agents still need it. The Access service token and Temote host bearer token are independent credentials.

```sh
(cd fabric && npx wrangler secret put HOST_TOKENS_JSON)
```

5. Generate and check metadata, run tests, then bundle without publishing. After remote configuration and secrets are verified, deploy with the selected target:

```sh
# Repository root
just generate-tools
just check-generated

# fabric/
(cd fabric && npm test)
(cd fabric && npx wrangler deploy --dry-run --keep-vars)
(cd fabric && npx wrangler deploy --keep-vars)
```

6. Verify health and authenticated MCP reachability using the checks below. A deploy without a target can upload a version without publishing it.

The public MCP URL is `https://<gateway-host>/mcp`. The Worker imports `fabric/contract/routed-tool-metadata.json` generated from Rust; see [generation and stale checks](development.md#connected-runtime-contract-parity). A bundle dry-run proves configuration/build validity, not remote authentication, secret presence or endpoint readiness.

The D1 schema includes observation, memory-run, checkpoint, outbox, and knowledge tables. Inspect all pending migrations before applying them. Migration `0003_memory_worker.sql` rebuilds the knowledge tables while copying existing rows and provenance, so review it against the target database before applying. Memory extraction is disabled by default (`MEMORY_ENABLED = "false"`). To enable the OpenAI-compatible extractor, set `MEMORY_ENABLED = "true"`, `MEMORY_EXTRACTOR = "openai_compatible"`, a full `MEMORY_ENDPOINT` URL, `MEMORY_MODEL`, `MEMORY_TIMEOUT_MS`, `MEMORY_INPUT_BUDGET_BYTES`, `MEMORY_OUTPUT_BUDGET_BYTES`, `MEMORY_MAX_ATTEMPTS`, `MEMORY_BATCH_SIZE`, and `MEMORY_PROJECTION_GENERATION` in the existing Worker config. Store `MEMORY_API_KEY` as a Worker secret; never put its value in `wrangler.toml`, `.dev.vars.example`, or an evaluation artifact. Increase `MEMORY_PROJECTION_GENERATION` monotonically when changing extraction semantics so an older worker cannot replace the active projection.

Optionally set `MEMORY_REASONING_EFFORT` to `low`, `medium`, `high`, `minimal`, `none`, `max`, or `xhigh`. When unset, the adapter omits `reasoning_effort` so the provider uses its default. A value outside this local allowlist reports `provider_configuration_invalid` without sending a request. An allowlisted value unsupported by the selected provider reports `provider_rejected` for its HTTP 400 response. OpenCode Go lists GLM-5.3-Flash as `glm-5.3-flash`; the GLM API documents that thinking cannot be disabled for GLM-5.3-Flash, its default effort is `max`, and `low` is supported. For bounded live extraction with that model, explicitly select `low` ([OpenCode Go models and endpoints](https://opencode.ai/docs/go/), [GLM Chat Completion parameters](https://docs.z.ai/api-reference/llm/chat-completion)). The producer version tracks the adapter version, selected effort, and envelope limit. The adapter version advances from 4 to 5 for the admission-bound and same-text promotion fixes; prompt version 4 stays unchanged while policy version advances from 6 to 7.

`MEMORY_OUTPUT_BUDGET_BYTES` continues to cap the extracted JSON content. Separately, the HTTP response JSON envelope is capped at `min(80 KiB, 2 × MEMORY_OUTPUT_BUDGET_BYTES + 16 KiB)`: 32 KiB at the default 8 KiB content budget and 80 KiB at the 32 KiB maximum. The adapter reads only the response `content`; provider `reasoning_content` and `usage` are ignored, never logged or persisted, and never reused. If the provider response includes `finish_reason`, only `stop` is accepted; any other value is rejected without advancing the checkpoint. A genuinely absent `finish_reason` is allowed. An oversized envelope also fails without advancing the checkpoint. After correcting the provider or response-budget configuration, increase `MEMORY_PROJECTION_GENERATION` to retry retained observations as a new projection.

```sh
(cd fabric && npx wrangler secret put MEMORY_API_KEY)
```

`wrangler secret put` creates a Worker deployment immediately. Run it only as part of configuring the intended target, after checking the target and Access policy. See [Wrangler secret management](https://developers.cloudflare.com/workers/configuration/secrets/). Set `MEMORY_ENABLED` to `false` to pause extraction or `true` to enable it, then deploy the reviewed Worker config. The `temote-memory` Queue and five-minute scheduled sweep are declared in `fabric/wrangler.toml`; preserve both bindings when deploying. Queue messages are at-least-once wake-ups; the D1 outbox marks unsent or stale work due after 60 seconds, and the next five-minute sweep requeues it, including recovery when a Queue send response was lost. A provider run stops retrying after `MEMORY_MAX_ATTEMPTS` and reports `failed` with a bounded error code through `context_status`; the sweep does not override that limit. To retry after fixing a provider or input failure, monotonically increase `MEMORY_PROJECTION_GENERATION` and deploy the reviewed config. This requests a new producer version that reprocesses retained observations from the start while the prior active projection remains readable until the rebuild catches up. Treat this as an explicit rebuild with provider cost. Use `context_status` to inspect synchronization and worker freshness after configuration. Do not enable the `fixture` extractor in a live deployment.

Changing the adapter version isolates new runs from completed runs under the previous producer version, including runs that were incorrectly marked complete. Before deploying the version 5 adapter, monotonically increase `MEMORY_PROJECTION_GENERATION` to rebuild from retained observations; raw observations are unchanged. Until the new projection is published, the prior valid projection remains readable but is marked stale. Use `context_status` to distinguish that rebuild state from a complete current projection.

The Memory Worker processes at most one repository group per invocation. Its conservative worst-case budget is 602 D1 statements: up to 14 setup and claim statements; 48 projection lookups (at most 24 knowledge items, including generated summaries, each with an active-row and support-detail read); 24 supersession-provenance reads; a commit batch capped at 512 statements; and four post-commit or pending-outbox recovery statements (one post-commit read and three outbox-recovery statements). The support-detail read replaces an earlier count read and does not increase the total. The 602-statement bound is below the current 1,000-query-per-invocation limit for Workers Paid in [Cloudflare D1 limits](https://developers.cloudflare.com/d1/platform/limits/). The same worst-case batch exceeds the Free limit of 50, so Free worst-case qualification is not claimed. This local statement-count qualification does not verify a deployed account, plan, or remote Worker.

If projection preparation would exceed the 512-statement commit cap, the Worker returns `projection_too_large` without committing knowledge or advancing the checkpoint; retries stop at `MEMORY_MAX_ATTEMPTS`. Reduce `MEMORY_BATCH_SIZE` to keep the input within the cap, then increase `MEMORY_PROJECTION_GENERATION` and deploy the reviewed config to rebuild from retained observations. The previous active projection remains readable until the new projection is published.

## Deployment target

`workers_dev = false` means a deploy without a route or custom domain does not publish the Worker and can print `No targets deployed`. Treat that output as a failure even when the command exits 0, and choose exactly one target:

Before invoking Wrangler, run the repository-local preflight with the intended target. This command runs from `fabric/`:

```sh
cd fabric
npm run deploy:preflight -- --hostname gateway.example.com --route 'gateway.example.com/*'
```

The preflight reports `target_missing`, `target_mismatch`, or `remote_unknown` as distinct statuses. `remote_unknown` is intentional: this local check does not use Cloudflare credentials and never claims that a deployed route or custom domain exists. Treat every non-zero result as a deployment stop until the target is corrected and the post-deploy read-only verification below succeeds.

| Option | Use when | Target setup | Access | Verification |
| --- | --- | --- | --- | --- |
| A. Custom domain | The Worker should own a dedicated hostname, or no DNS record exists yet. | Declare the hostname as a Worker custom domain, for example `routes = [{ pattern = "<gateway-host>", custom_domain = true }]` in `fabric/wrangler.toml`, or create it in the Cloudflare dashboard. | Protect the whole hostname with a Cloudflare Access application. | Run Wrangler status from `fabric/`; use the Access-authenticated `/healthz` check below. |
| B. Existing DNS + Worker route | A DNS record already exists and must not be deleted. | Pass the exact pattern to the deploy, for example `npx wrangler deploy --keep-vars --routes '<gateway-host>/*'`. | Protect the whole hostname with a Cloudflare Access application. | Same as A; also confirm the route pattern points at `temote-mcp-gateway` in the Cloudflare dashboard. |

Both options follow the same rules:

- Keep `workers_dev = false`. Do not enable the `*.workers.dev` route as a shortcut.
- Do not delete existing DNS records. A Worker route takes precedence for matching requests without removing the record, so the direct origin stays recoverable.
- Use `--keep-vars` when the deployment must preserve dashboard-managed non-secret variables. Worker secrets are unaffected and stay in `wrangler secret`; never write them to `wrangler.toml`, docs, or issue trackers.
- Keep the Access service-token credential and the gateway host token as separate credentials.
- A successful upload is not a successful deployment. Verify the target after every deploy.

### Verify a deployment (read-only)

Run the Wrangler command from `fabric/`. The whole hostname is protected by Access, so send the service-token credentials accepted by its Service Auth policy for the health check. Keep these values in a protected environment or secret store; do not paste them into the command or logs:

```sh
(cd fabric && npx wrangler deployments status --name temote-mcp-gateway)
curl --silent --show-error --fail \
  --header "CF-Access-Client-Id: ${TEMOTE_FABRIC_ACCESS_CLIENT_ID:?set the Access service-token client ID}" \
  --header "CF-Access-Client-Secret: ${TEMOTE_FABRIC_ACCESS_CLIENT_SECRET:?set the Access service-token client secret}" \
  "https://<gateway-host>/healthz"
```

`/healthz` must return the Temote gateway identity and `readiness=ready` (currently `{"status":"ok","service":"temote-fabric","readiness":"ready","identity":"temote-fabric","compatibilityIdentity":"temote-mcp-gateway","contractFingerprint":"<sha256>"}`). The Worker does not require a client token for `/healthz`; Cloudflare Access still protects the hostname at the edge. A direct-origin response or a different service identity means the hostname still points at the wrong target. `contractFingerprint` is the SHA-256 digest of the public tool contract; it must equal `fabric/contract/public-tools.fingerprint` from the deployed source revision and the `server_contract_fingerprint` reported by the local/connected server's `session_info`.

For MCP `tools/list` and other `/mcp` requests, use an MCP client authenticated through Access Managed OAuth as a user whose email is in `ACCESS_ALLOWED_EMAILS`. The Worker verifies the Access JWT's signature, audience, issuer, expiry, subject, and allowlisted email. An Access service token is for host-agent Service Auth and the `/healthz` smoke check; its JWT has no user email and an empty subject, so it does not satisfy `/mcp`'s user-identity check. Do not set the local/test `CLIENT_TOKEN` on the production Worker.

### Rollback

Rollback removes only the exact Worker route or custom domain that was added:

1. Remove the exact route pattern or custom-domain binding for `<gateway-host>` in the Cloudflare dashboard or by reverting `fabric/wrangler.toml`, then deploy the previous target set.
2. Leave the DNS record, the Access application, and the Tunnel untouched so the direct origin remains usable.
3. Confirm with `curl -sSf https://<gateway-host>/healthz` and the dashboard that the hostname no longer resolves to the Worker, then restore the intended direct-origin configuration if needed.

Do not remove or rewrite unrelated routes, DNS records, or Access policies as part of a gateway rollback.

## Configure each Temote host

Configure named roots on the machine running the supervisor. Only root names are advertised to the gateway; physical paths remain local.

### Browser-enrolled Host

The browser flow uses the running Supervisor's configured roots and never adds or remaps a root. Save the public connection settings once, then use the CLI directly:

```sh
temote fabric configure \
  --gateway-url https://<gateway-host> \
  --oauth-issuer https://<configured-access-issuer> \
  --oauth-client-id <registered-public-client-id> \
  --oauth-callback-port 8400
temote fabric check
temote fabric connect
# In a separate terminal:
temote fabric status
```

`configure` saves only public settings in the owner-only platform config directory, under `temote-mcp/fabric/public.json`. It never logs in, registers a client, changes Supervisor roots, or saves credentials. Explicit CLI options take precedence over canonical/legacy environment settings, which take precedence over the saved public profile. A saved profile selects browser mode; `connect --browser` explicitly selects it and ignores static credential environment variables. An explicit `--host-token` or `--static` retains the static Link workflow; `status --static` retains its diagnostics. `configure` with complete settings can replace malformed contents if the existing file metadata is safe.

`check` reads restricted Supervisor diagnostics, validates the expected Host and configured root names, briefly binds and releases the callback port, and checks public OAuth discovery. It does not start authentication or read/write secure credentials. It does not prove filesystem root identity; the existing admission protocol verifies that identity during connect.

`connect` opens the system browser and receives the callback automatically. Complete login, MFA and consent in the browser. Never copy an OAuth code into Terminal. The CLI displays the verified account and the Host obtained from the running Supervisor. Use arrow keys and Enter to choose **Approve** or **Cancel**; Cancel is initially selected. Then use arrows and Space to select the existing roots and choose **Approve selected root scope**. No root is selected automatically, and Host IDs, root names and `yes` are never typed. Escape, Ctrl-C or EOF cancels; pasted/text input is discarded without echo and requires a new selection. Only after both approvals does the CLI save the enrollment recovery record, reserve/activate the Host grant and start the outbound Link. Keep the foreground connect terminal open, or use the managed background option below.

Browser `status` uses a noninteractive secure-store read and read-only account, enrollment and Link queries. It never logs in, refreshes credentials, alters grants, takes enrollment locks or reconnects. It separately reports authentication, enrollment, Link state and approved roots; an empty session inventory does not make an online Link offline. Full readiness also requires read-only root identity verification. On an older Supervisor, restricted Ping still permits account/grant/Link confirmation, while `root_identity` is `unverified` and `ready` remains false. Browser Link confirmation also requires a local connection receipt bound to the current gateway and grant ID/generation; an older receipt without that binding reports `generation_unverified` until a successful interactive reconnect. These commands never automatically upgrade or restart a Supervisor.

For an owner-managed connection that survives closing Terminal, run `temote fabric connect --background`. Initial browser and account/root approvals still happen in that terminal. The command returns only after the detached child has verified the saved grant, current root identity and first connection. Use `temote fabric status` to inspect its `background` state, and `temote fabric disconnect` to stop this managed Link. A stop acknowledgment reports `stopping`; successful completion confirms that the requested instance ended. The Supervisor, sessions and saved grant remain available. Foreground and background Links share one profile lock, so stop the foreground Link with Ctrl-C before switching. Static-token background mode is unsupported.

The detached child never starts login or approval UI. It stops if its grant, owner, root identity or Supervisor epoch changes, or if authentication/refresh cannot be verified. A temporary verification read outage pauses new requests and retries under the same grant for up to 60 seconds; authentication rejection and uncertain refresh halt immediately. macOS refresh updates only the existing Keychain item with interaction forbidden and verified read-back. Linux currently stops before refresh when safe noninteractive persistence is unavailable; recover with foreground `connect`. No launchd/systemd service or login item is installed. See [background Link lifecycle](designs/fabric-background-link.md).

When an older Supervisor reports an online Link but `root_identity: unverified`, connectivity has been confirmed and full readiness still needs an upgraded Supervisor. Review `temote upgrade --dry-run`, then apply the upgrade locally, restart and reconnect. Background startup requires the newer read-only root inventory and fails before login if it is unavailable.

#### First connection on multiple machines

Use the same Fabric URL, issuer and registered public client on each machine. Each machine runs its own Supervisor with its actual unique Host identity and local named roots, saves its own public settings, signs in, and approves its own Host/root grant. `--host-id` pins the expected running Supervisor identity; it does not rename that Supervisor. Check the Host displayed by `fabric check` before enrolling. Reusing one Host ID on two machines causes generation replacement rather than creating two discoverable Hosts. Select an explicit `host_id` in the client after both appear in `host_list`. The same fixed loopback callback port can be used on different machines. See the host/session routing examples below.

The callback normally binds an available IPv4 loopback port dynamically. If the OAuth client requires a pre-registered redirect URI, pass `--oauth-callback-port 8400` (or set `TEMOTE_FABRIC_OAUTH_CALLBACK_PORT=8400`) and register `http://127.0.0.1:8400/callback`. The selected port must be available; a bind conflict stops before the browser opens, with no fallback port. The same redirect URI is used for authorization and token exchange. The option applies to browser `connect`/`link`; logout and static Host-token mode do not use it.

The RFC 8707 resource defaults to the Fabric origin; set `--oauth-resource` only when the Access configuration requires another explicitly reviewed resource value. The CLI uses macOS Keychain or an available Linux Secret Service to persist one atomic credential record. Linux and Windows 11 under WSL2 require an already available Secret Service; other or unavailable secure stores fail closed. Browser mode uses the opaque OAuth bearer for Access and a separate Host-grant header for Worker authorization. It does not use the static host-token or Access Service Token variables below.

Run `temote fabric logout` with the same gateway, issuer, client, and Host settings to revoke the remote Host grant before removing the secure credential record. If revocation cannot be confirmed, credentials stay stored so the operation can be retried. The browser enrollment grant expires after 90 days; reconnect verifies an existing saved profile and refreshes OAuth without another browser login when the grant remains valid. Reauthorization or root changes require explicit confirmation.

The first browser-authenticated release exposes owner-filtered live Host/session operations under the approved roots. Cloud context, dashboard data, Fabric data, interaction, mentions, and observation replicas remain unavailable to browser identities until their owner/root namespace can be verified.

### Existing static Host-token mode

`temote fabric connect` and `temote fabric link` are aliases for the same lifecycle. With `TEMOTE_FABRIC_HOST_TOKEN` (or a supported legacy alias) configured, either command runs the existing static per-host token Link; without a Host token, either command starts browser enrollment. The static mode accepts the optional Access Service Token pair only when both values are configured. A service-token value without a Host token, or an incomplete pair, is an error. The existing `temote-mcp gateway-agent` command remains available for explicit static compatibility.

macOS example:

```sh
export TEMOTE_ROOTS='{"src":"/Volumes/devstorage/Developer","work":"/Users/me/work"}'
export TEMOTE_FABRIC_HOST_ID=mac-main
export TEMOTE_FABRIC_URL=https://<gateway-host>
export TEMOTE_FABRIC_HOST_TOKEN='<token assigned to mac-main>'
export TEMOTE_FABRIC_ACCESS_CLIENT_ID='<Access service-token ID>'
export TEMOTE_FABRIC_ACCESS_CLIENT_SECRET='<Access service-token secret>'

temote-mcp supervisor
temote fabric link --host-id mac-main
```

Run the supervisor and static `fabric link` in separate foreground terminals. `temote fabric link` is also available as `temote-mcp gateway-agent`; both use the same reconnecting Host protocol. Existing `TEMOTE_MCP_GATEWAY_*` environment names remain supported as fallback aliases, while `TEMOTE_FABRIC_*` takes precedence. Check readiness with `temote fabric status`: it prints configuration, supervisor, remote endpoint identity, authorization, host lease/generation, and publicly usable session stages. It is read-only and exits unsuccessfully unless every stage is ready; credentials and remote response bodies are never printed. It uses a restricted local diagnostic request; if the running supervisor does not support that request, the local stage is unavailable and the CLI does not send a legacy maintenance probe.

Linux uses the same model with Linux paths. On Windows 11, run the supervisor and agent inside WSL2 and use WSL paths such as `/mnt/d/Developer`:

```sh
export TEMOTE_ROOTS='{"src":"/mnt/d/Developer"}'
temote-mcp supervisor
temote fabric link --host-id win-main --platform wsl2
```

The existing host-level `gateway-agent --host-id` automatically sends bounded observation batches over its authenticated channel: at most 32 records or 512 KiB per batch, on a five-second loop with an eight-second sync request timeout. It writes locally first, so a Fabric timeout or authorization failure does not fail a delegated task or replay its backend operation. The agent scans retained session journals, including terminal observations created when the ordinary `task_get` polling observes completion, and persists an owner-only per-host, per-Fabric-endpoint, per-session cursor. It does not independently poll a backend after its caller stops polling. `committed_through_revision` records the highest source revision confirmed committed to D1; `acked_through_revision` is the contiguous source revision D1 can confirm. The delivered cursor can advance across a known gap so later retained records still sync, while the gap remains visible and `complete=false`. The source revision cursors are not cloud sequence numbers.

Sync diagnostics persist as owner-only status data with stable error codes and timestamps. Retry delay grows from two seconds to a five-minute maximum and resumes after agent restart. `context_status({repository: ...})` reports cloud cursors, gaps, and whether a source is partial. No manual sync command is needed. There is no cursor-reset CLI. If an operator-approved repair restores an older missing journal record, stop only the matching host's `gateway-agent`, set `cursor` to that host/endpoint/session's `ack-<session-id>.json` under `gateway-observation-sync/<host-id>/<sha256-normalized-gateway-url>/`, then move that one file to a unique owner-only backup name:

```sh
mv -- "$cursor" "${cursor}.backup-${unique_timestamp}"
```

Restart the same agent to rescan retained records. Keep the observation journal, `status.json`, and every other cursor. Exact D1 replays are idempotent; rescan does not by itself erase a previously reported source gap or establish completeness. Verify the result with `context_status`.

Repository-wide context is enabled only when the workspace has one unambiguous, supported Git `remote.origin.url`. Fabric normalizes GitHub, GitLab.com, and Bitbucket.org remotes to a path-free key such as `github:owner/repository`; it never uses the checkout path or directory name. If the remote cannot be safely resolved, records remain session-scoped and do not contribute to repository-wide current knowledge.

Instruction and error previews are excluded from cloud sync by default. `TEMOTE_MCP_OBSERVATION_SYNC_PREVIEW=1` opts into sending bounded text previews. These previews may contain sensitive free text; the size bound does not scrub secrets. Enable this setting only when the existing content-sharing policy permits those previews.

Keep this setting unchanged until every pending batch for that host and Fabric endpoint is ACKed. D1 may commit a batch while its response is lost; after restart, the host retries from its durable cursor. If the preview setting changed, the retried payload differs and D1 rejects it with `409 conflicting_replay` rather than overwriting the committed observation. An opt-out retry omits previews. Do not weaken payload digest checks to make the retry appear successful. If a conflict occurs, restore the originally authorized preview policy and sync until ACK only when that upload remains authorized; otherwise stop that host agent and leave the source diagnosed. Turning previews off cannot undisclose preview text already committed to D1.

`--platform auto` detects macOS, Linux, and WSL2. When `TEMOTE_FABRIC_HOST_ID` is configured, `temote-mcp doctor` reports staged gateway readiness: each `local_config` item (host ID, gateway URL origin, host token presence, Access service-token pair) and the `local_supervisor` control protocol are separate results. With the network-enabled build it also performs an Access-authenticated `/healthz` identity check and `/v1/hosts/status` probe without following redirects. These classify the remote endpoint, authorization, and this host's active lease and generation. Doctor also reports `session_availability` from the local supervisor's read-only session inventory as `listed_sessions`/`active_sessions`/`publicly_usable_sessions` counts; yolo/local-only sessions and sessions that are still starting do not count as publicly usable. This reuses a restricted supervisor diagnostic request, never dispatches an MCP tool, and never mutates a session or lease. An older running supervisor that does not support this request is reported as unavailable without receiving a legacy maintenance probe. An inventory with no active non-yolo session is reported as `failed`, not `ready`; an inventory that cannot be enumerated is `unavailable`. Doctor requires the gateway's active lease and generation to match the local host-agent record before reporting registration as ready. The host-level agent reports a bounded `session_availability` value (`ready`, `session_unavailable`, or `unavailable`) on each poll; an unreported value stays `not_checked`. Doctor never prints root paths, credential values, or remote response bodies.

## MCP workflow

The gateway adds host discovery and host-aware lifecycle routing:

```text
host_list()
host_info(host_id="linux-main")
session_list(host_id="linux-main")
session_start(host_id="linux-main", path="src/project-a", session_id="project-a")
session_info(host_id="linux-main", session_id="project-a")
```

`session_start` accepts either a named-root-relative logical `path` or a typed repository `source` with a UUID `operation_id`, never both. Source provisioning returns a durable readiness/reconciliation receipt; see [managed provisioning](managed-provisioning.md). The host-side public supervisor always creates a normal sandboxed session; a remote client cannot request `--yolo`.

The same human-friendly session ID may exist on multiple hosts:

```text
mac-main / srmj
linux-main / srmj
```

Use explicit `host_id` for deterministic routing. For backwards compatibility, an omitted `host_id` may resolve an existing `session_id` only when exactly one currently discoverable owner exists. If two hosts own the same ID, or ownership cannot be determined safely because a leased host cannot be queried, the gateway fails closed instead of choosing a host.

`session_stop` and `session_restart` apply only to active sessions owned by the host's public supervisor. Separately started local CLI/yolo sessions remain local-only: public session-bound tools reject yolo targets rather than inheriting their unrestricted semantics.

### Repository context and memory

Fabric also exposes the existing `context_resolve` and `context_status` tools. A repository request can omit `session_id` and works from the authenticated D1 replica while every execution host is offline:

```text
context_resolve({repository: "github:owner/repository", query: "report", budget_bytes: 16384})
context_status({repository: "github:owner/repository"})
```

The authenticated Worker configuration supplies the owner scope; callers cannot select another owner. Repository keys come from a stable forge identity, not a local directory name. A session request is checked against its recorded owner, host, and repository mapping. If a session has no cloud mapping, the existing host fallback remains available; an authorization or ownership error never falls through to another host.

`context_resolve` returns replicated task observations, derived current knowledge with support references, recent related tasks, and bounded freshness metadata. It labels observations `replicated_observed` and knowledge `derived`; it does not claim that replicated state is live. `context_status` reports source cursors and gaps, observation freshness, and Memory Worker readiness and lag. A partial or stale result is marked explicitly. Responses are bounded, deterministic, and do not include raw observation bodies. The `MEMORY_ENABLED = "false"` default still permits repository context from observations, with memory reported as disabled. Other host-routed and local session-bound tools continue to require their existing `session_id` contracts.

A repository with no owner-scoped replication source records is marked partial and stale. A recorded source with zero observation rows is evaluated from its cursor and gap state; a healthy zero high-water mark can be fresh. Worker status `ready_empty` describes extraction state separately from source freshness.

#### Knowledge promotion and conflicts

The Worker validates each support reference against an eligible observation and requires the quoted text to occur verbatim in that source. Extractor output cannot choose its own status or scope, and it must leave `verification_path` null. Confidence or an observation kind does not establish verification. A directly supported assertion is normally `supported`; only checked user instructions can promote selected items to `current`:

- A constraint becomes repository-wide `current` only when a user instruction uses an explicit repository-policy marker, includes a direct constraint clause, and the quoted clause is supported by that instruction. Task-specific constraints stay task-scoped; other ordinary constraints stay at their narrower task, workspace, or execution scope (including an operation-scoped execution) and remain `supported`.
- A directly quoted user decision can be `current` at its derived task, workspace, or execution scope.
- Facts, observations, failure patterns, and agent statements remain `supported`. A completed task or an agent statement such as “tests passed” describes what was reported or observed; it does not verify that the requested change is correct.

After a successful real extractor call and validation of its complete response, the Worker also deterministically projects exact clauses from a repository declaration block. The first nonblank line must start with a supported header, such as `For this repository, the repository-level policy is:` or `For this repository, the repository-level policy has changed:`; a direct constraint clause must follow on the same or next line. An immediately following `Open question:` line is optional, as is an immediately following `Previous repository-level policy to replace:` line. These lines belong to the same block. Quoted or fenced clauses, lines indented by four spaces or a tab, and unsupported header forms are not accepted. Any narrative line ends the block permanently; later headers or predecessor text are ignored. The predecessor is an input-only authority hint for supersession, never a knowledge claim.

For example, the declaration prefix in Task A can be:

```text
For this repository, the repository-level policy is:
Report output format must be JSON.
Open question: The required report field set remains undecided.
```

The changed declaration prefix in Task B can be:

```text
For this repository, the repository-level policy has changed:
Report output format must be TOML.
Open question: The required report field set remains undecided.
Previous repository-level policy to replace: Report output format must be JSON.
```

The exact quoted Task A policy in Task B authorizes supersession; the predecessor text itself is not stored as a claim. A repository-level constraint from this block can become `current`, while its unresolved clause is stored as `supported` and can be retrieved as `supported` or `current`. Task-specific statements do not become repository-wide policies. Provider failure or invalid model output cannot be rescued by this deterministic projection; neither case commits a projection or advances its checkpoint. Additional extractor proposals remain model-derived and go through the same support, scope, and output validation.

A changed repository policy supersedes an older current policy only when the new instruction either quotes the exact predecessor with `Previous repository-level policy to replace: ...`, or comes from the same host and session at a newer source revision than every direct instruction supporting the old policy. Otherwise the Worker keeps the old item `current` and stores the conflicting item as `supported`; it does not use last-write-wins. A generated summary repeats its source quote and is marked as summary provenance, not as independent evidence.

Support provenance is bounded to 16 distinct direct or summary references per knowledge item. If additional valid references cannot be persisted, the Worker retains the bounded set and sets the sticky `support_incomplete` flag; migration also marks existing items whose support rows exceed the cap. It does not present the saved references as complete. `context_resolve` includes that flag on the affected item (`knowledge_summary_support_incomplete` for its summary) and sets `partial.value` with the `knowledge_support_incomplete` reason when selected knowledge or its supersession history has incomplete support. An incomplete old policy cannot be superseded through source-revision ordering alone. An authenticated same-scope change instruction can still supersede it by naming the exact predecessor text.

The default sync policy omits instruction and error previews, so a configured and caught-up Worker can report `ready_empty` when it has no active supported or current knowledge items. This is a successful empty projection, distinct from `disabled`, `not_configured`, `failed`, or `lagging`; observations can still be returned by `context_resolve`. Bounded preview opt-in can provide more extraction input, but previews may contain secrets as described above.

Per-source `source_head_revision` and `source_acked_revision` are journal cursors. `latest_cloud_seq` is a separate D1 cursor; do not compare them as if they shared one counter. A gap or unacknowledged source revision makes the observation context partial.

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

For local Worker development, copy `fabric/.dev.vars.example` to `fabric/.dev.vars`. Never commit `.dev.vars`, Worker secrets, Access service-token secrets, host bearer tokens, or endpoint environment files.

The routed tool schemas and MCP protocol versions are checked against a Rust-generated contract snapshot in both Rust and Node tests. `serverInfo.version` is the Cloudflare deployment revision from the `GATEWAY_DEPLOYMENT` version-metadata binding, not the Temote CLI CalVer.
