# Temote and Fabric naming migration

The crates.io package remains `temote-mcp`; `temote` is the canonical CLI, and `temote-mcp` remains an executable compatibility alias. Plugin/skill IDs, local state directories, socket locations, and persisted task ownership remain compatible. A source rebuild retains the repository's baseline version; only the CalVer release workflow allocates a release version.

## Host configuration

`TEMOTE_` names take priority over corresponding `TEMOTE_MCP_` names. Fabric Link first reads `TEMOTE_FABRIC_`, then `TEMOTE_GATEWAY_`, then `TEMOTE_MCP_GATEWAY_`. Lookup never copies credentials into another environment variable. Child processes scrub all supported credential aliases unless explicitly allowed by their backend.

```sh
temote fabric connect
temote fabric status
temote fabric events-sender --addr 127.0.0.1:4211
```

`fabric link` and `gateway-agent` run the same outbound Link. Existing CLI option names remain supported. Configure `TEMOTE_FABRIC_URL`, `TEMOTE_FABRIC_HOST_ID`, and the Link credentials through runtime injection. `TEMOTE_CODEX_BINARY` selects a validated Host-local Codex executable when a service's PATH lacks it; public task tools cannot select an executable. The Host validates the resolved executable target and preserves the configured invocation path, so an authorized multicall launcher can retain its `codex` applet name. A native Codex executable is also supported. The dedicated sender requires injected `TEMOTE_EVENTS_SENDER_BEARER` and must run behind an Access-protected Tunnel. Use the 1Password MCP server for Developer Environment authorization and mounted runtime files; do not display their contents.

Upgrade only after delegated tasks and required evidence reads have finished. Inspect `temote upgrade --dry-run`, including in-flight and restore blockers, before applying. Keep the original installed executable and migration-aware configuration for rollback. A successful execution does not imply task verification or delivery passed.

## Non-destructive Fabric deployment

`cloudflare.config.ts` reads a non-secret, ignored JSON profile from `TEMOTE_DEPLOYMENT_CONFIG`. Real domain, D1 ID, policy identifiers and migration stage belong there. Secrets remain runtime bindings. Profiles reject secret-shaped variable names. Declare any existing `CLIENT_TOKEN`, `HOST_TOKEN` or `FABRIC_INTERACTION_SECRET` binding by name in `compatibilitySecrets`; inspect binding names without reading values before migration. The frontdoor retains these at the source authority. `cf` builds and deploys the same pinned source and asset bundle.

The deployment plan uses these explicit stages, with configurable source and target Worker names:

| Stage | Worker | Code / state |
| --- | --- | --- |
| `legacy` | compatibility source | Existing classes and namespace bindings. |
| `rename` | compatibility source | Cloudflare class rename tombstones plus live `FabricSession` / `FabricRegistry`; namespace IDs must remain unchanged. |
| `prepare` | canonical target | `expecting-transfer` declarations; no public domain move. |
| `transfer` | compatibility source | Transfers the existing namespaces to the target; source authentication remains in place and bindings address the target. |
| `fabric` | canonical target | Live transferred classes and public domain; Service binding forwards requests to the retained credential authority. |

The frontdoor forwards the original streaming request once. It never retries a mutating call, rewrites credentials, grants approval, or interprets task output. The compatibility Worker continues authenticating every request under its existing policy; the Host retains execution authority. Canonical and legacy DO bindings address the same namespaces. Only one version-metadata binding is declared, with legacy reading supported by the protocol adapter.

Capture a fresh `cf durable-objects namespaces list` inventory and run `node scripts/cf-deployment-preflight.mjs PROFILE BASELINE INVENTORY`. The non-secret baseline includes `databaseId` and the two retained namespace IDs. This validates local prerequisites and reports remote deployment as NOT RUN.

Before each stage, build and run `cf deploy --prebuilt --dry-run`; inspect the plan and namespace IDs. Apply append-only D1 migrations to the selected database after review. Deploy the source candidate first, then the class rename, target preparation, source transfer and target activation. Confirm the two namespace IDs are identical to the recorded baseline at every transition. Keep the domain and Access application/audience stable until target activation; probe health, authentication denial and routed Host/session reads afterward. Do not delete or recreate a namespace, database, credential authority, or existing session to make migration succeed.

```sh
# From the current Fabric source directory; choose the reviewed profile.
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf build
TEMOTE_DEPLOYMENT_CONFIG=.cloudflare/deployment.json cf deploy --prebuilt --dry-run
```

Before transfer, rollback can retain the source namespace owner. After transfer, rollback must retain target-owned namespace bindings and the migration-aware compatibility Worker; do not deploy a historical configuration that creates old classes again. Moving the public domain back to the compatibility Worker remains possible without moving or recreating state. Credential migration/removal is a separate later operation, not required to retain compatible service authentication.

The source directory is `fabric/`. Its rename follows successful available-environment runtime/deployment proof. After target activation, the retained authority uses the `transfer` profile without a public domain, so later authority deployments cannot move the domain back implicitly. External sender, registered client, physical multi-host and provider gates are recorded separately as NOT RUN when unavailable. A build, fixture or dry-run is never reported as a live migration PASS.
