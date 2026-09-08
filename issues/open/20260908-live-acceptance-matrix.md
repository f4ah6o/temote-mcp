# TEMOTE live acceptance matrix

- Status: Open
- Date: 2026-09-08 (Asia/Tokyo)
- Priority: P2
- Type: validation / deployment tracking

## Purpose

Keep external, credential-dependent, destructive, and physical multi-host acceptance work out of otherwise-complete implementation issues.

This file is the single tracking issue for live evidence that cannot be established by repository-local unit/integration tests alone. It must not be used to keep completed implementation work artificially open.

## Source issues consolidated here

- `20260806-reconnect-oauth-example-domain.md`
- `20260807-cloudflare-workers-durable-object-gateway.md`
- `20260823-ingress-auth-provider-profiles.md`
- `20260823-installed-runtime-migration.md`
- `20260908-multi-host-federation.md`

The source issues are archived under `issues/done/` after this consolidation.

## Acceptance matrix

### Cloudflare direct ingress / Managed OAuth

- [ ] A real Cloudflare Access + Tunnel deployment is configured with the production profile and current secrets/configuration.
- [ ] An authenticated external MCP client completes `initialize` / `tools/list` through Cloudflare Managed OAuth.
- [ ] ChatGPT connects to the configured MCP endpoint and exposes the expected Temote tools after authentication.
- [ ] Managed session start/stop is exercised through the live Cloudflare path with the normal sandbox/approval boundary intact.

### Cloudflare Worker / Durable Object gateway

- [ ] Durable Object exports/configuration and required secrets are applied to a real Cloudflare account.
- [ ] At least two concurrent gateway sessions are exercised through one external MCP endpoint.
- [ ] Reconnect/generation fencing is observed live; stale generations are rejected and non-idempotent calls are not auto-replayed.

### Connection profile matrix

- [ ] Cloudflare profile external live acceptance is re-run on the current release.
- [x] Tailscale Funnel + local OAuth external live acceptance has repository-recorded evidence.
- [ ] OpenAI Secure MCP Tunnel connects from a supported OpenAI product to Temote without requiring a public Internet origin.
- [ ] Cloudflare, Tailscale, and OpenAI profiles exercise equivalent managed session start/stop semantics where the provider is available.
- [ ] Cloudflare, Tailscale, and OpenAI live paths preserve the same sandbox, approval, no-yolo, and named-root boundaries.

### Installed runtime migration

- [ ] On a disposable or otherwise explicitly accepted live host, a valid legacy `serve + cloudflared` pair referenced by legacy `up.pids` is stopped by `temote-mcp migrate` only after process identity verification.
- [ ] The destructive acceptance confirms unrelated local sessions/processes remain untouched.

### Multi-host federation

- [ ] One configured MCP endpoint concurrently exposes a macOS native host, a Linux native host, and a Windows 11/WSL2 host.
- [ ] Each host runs one supervisor and one host-level gateway agent.
- [ ] A remote client starts and addresses sessions on each selected host using named-root-relative paths.
- [ ] The same `session_id` exists on at least two hosts without collision.
- [ ] An unqualified ambiguous lookup fails closed in the live deployment.
- [ ] Host disconnect/reconnect advances generation and stale agents cannot receive or submit work.
- [ ] Host-local sandbox and approval behavior remains unchanged on all tested hosts.

## Evidence requirements

For each checked item record enough evidence to identify:

- date and release/commit under test;
- provider/host type;
- command or client operation class without secrets;
- pass/fail result;
- relevant non-secret logs or test artifact reference;
- any environment limitation that makes the result conditional.

Do not store tokens, API keys, OAuth codes, cookies, raw credential-bearing environment, or secret-bearing logs in this issue.

## Completion rule

This tracking issue is complete when the applicable live matrix has evidence for the supported production paths. A provider that cannot be tested because entitlement/credentials are unavailable remains explicitly pending; it must not be silently substituted by another provider.

Repository-local implementation regressions belong in their owning implementation issue or tests, not here.
