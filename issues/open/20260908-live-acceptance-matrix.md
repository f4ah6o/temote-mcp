# TEMOTE live acceptance matrix

- Status: Open / tracking; external evidence pending
- Model: deepseek-v4.1-flash
- Date: 2026-09-08 (Asia/Tokyo)
- Updated: 2026-09-16
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
- [ ] On a real account, the chosen gateway deployment target (custom domain binding or existing-DNS Worker route) is applied with `workers_dev = false`, deploy output is checked so `No targets deployed` is not treated as success, and the route/domain binding is verified read-only (from `issues/done/20260911-gateway-deployment-target.md`).

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

### Developer broker / installed runtime

- [ ] On a host with Vite+ `vp` installed, a rebuilt Temote normal (`yolo=false`) session completes representative `dev_tool_run` `vp check`, `vp test`, and `vp build` operations without whole-session yolo or broad HOME exposure; record the installed version, operation class, and non-secret result.
- [ ] On a macOS host with a Vite+-managed Codex installation, a rebuilt Temote `local_agent_run(agent=codex)` completes after authorization with the verified bounded launcher dependency closure; record only non-secret launcher/runtime evidence and the result.

### Agent-mode development network

- [ ] In a rebuilt normal `agent` session, ordinary `execute`/`start_command` reach outbound HTTPS, a localhost client/server pair, and a reachable LAN HTTP/TCP fixture (RTSP where a fixture exists) with sandbox and path containment intact, while an `ask` session stays network-disabled (from `issues/open/20260915-agent-development-network-access.md`).

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

## Triage note

- 2026-09-16: Classified `keep-open` and kept in `issues/open/` as the single tracker. The unchecked matrix items require external credentials, real providers, physical multi-host hardware, and macOS/Windows hosts; they do not block repository-local implementation work. Unmerged branches contain bounded evidence that should be folded in when integrated (Cloudflare account/read-only deployment status and the OpenCode provider entitlement attempt on 2026-09-15). The OpenCode delegation implementation issue is complete and its provider entitlement remains tracked here.

## 2026-09-16 consolidation update

This is the single tracker for live-only evidence. It now also owns the remaining live checks formerly duplicated by the Vite+-Codex runtime issue, host-side release-trigger implementation issue, connected-surface drift issue, package-manager broker, repo-scoped GitHub credential routing, and rebuilt-runtime OpenCode canary. Repository-local implementation stays in the owning `doing`/`polished` packet; this matrix records only actual live/host/CI evidence.
