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
- [ ] On a real account, the chosen gateway deployment target (custom domain binding or existing-DNS Worker route) is applied with `workers_dev = false`, deploy output is checked so `No targets deployed` is not treated as success, and the route/domain binding is verified read-only (from `issues/done/20260911-gateway-deployment-target.md`). 2026-09-15 salvaged read-only evidence: `wrangler whoami` authenticated, `deployments status` reported one current `temote-mcp-gateway` version without proving a route/custom-domain target, `secret list` was empty (`HOST_TOKENS_JSON` absent under that account/script), and an unauthenticated `/healthz` returned an OAuth error document — reachability and auth enforcement, not an authenticated MCP session. See `docs/evaluations/completion-20260915-salvaged-evidence.md`.

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
- [ ] On a Linux host or outer-sandbox backend that supports nested user namespaces while retaining the Codex named-profile auth deny, a rebuilt Temote `local_agent_run(agent=codex)` completes a representative read-only canary and one bounded workspace write. 2026-09-15 salvaged evidence: on `ms-01-alpha` (`kernel.apparmor_restrict_unprivileged_userns=1` + `bwrap-userns-restrict`) the nested unified-exec returned `Operation not permitted` even for `pwd`, while the same installed Codex and named profile worked outside the outer sandbox — see `docs/evaluations/completion-20260915-salvaged-evidence.md`.

### Agent-mode development network

- [ ] In a rebuilt normal `agent` session, ordinary `execute`/`start_command` reach outbound HTTPS, a localhost client/server pair, and a reachable LAN HTTP/TCP fixture (RTSP where a fixture exists) with sandbox and path containment intact, while an `ask` session stays network-disabled (from `issues/open/20260915-agent-development-network-access.md`).

### Agent-mode Git shim

- [ ] On a rebuilt runtime, an installed Codex/OpenCode `local_agent_run` invokes ordinary `git switch` / `git switch -c` through the private shim and completes the branch change while direct `.git` writes remain denied (from `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`).

### Repository-completion live residuals (agent-mode consolidation)

Live evidence left open by issues closed at the 2026-09-22 consolidation; each row names its source issue. Record evidence per the requirements below before checking.

- [ ] macOS process-boundary upgrade reconnect E2E (`tests/upgrade_reconnect_e2e.rs`, currently `#![cfg(all(feature = "network", target_os = "linux"))] + #[ignore]`): run on a macOS host (from `issues/done/20260908-07-client-safe-upgrade-reconnect.md`).
- [ ] Codex delegation Phase-4 dogfood and Phase-D three-arm adoption evaluation on the current rebuilt runtime: T07-C, T08-C, T08-A, T09, T10 arms still unattempted; resume from the frozen manifest `docs/evaluations/completion-evaluation-manifest-20260915.md` (from `issues/done/20260908-08-codex-delegation-dogfood-and-app-server.md`).
- [ ] Local activity viewer S16 live gates: real supervisor activity stream with retention/disconnect evidence on a live host (from `issues/done/20260914-local-activity-viewer.md`).
- [ ] GitHub PR broker installed-runtime read-only canary plus live fixture close on a disposable repo (from `issues/done/20260916-github-pr-broker.md`).
- [ ] OpenCode `local_agent_run` read-only and bounded `workspace_write` canary against a rebuilt runtime (EPERM classification verified live) (from `issues/done/20260916-local-agent-opencode-eperm.md`).
- [ ] Codex `local_agent_run(model=..., effort="max")` live canary on a rebuilt runtime (from `issues/done/20260916-local-agent-reasoning-effort.md`).
- [ ] Package-manager broker live acceptance: `uv`, `npm`, `pnpm`, `go` supported dependency operations from a normal `agent` session (from `issues/done/20260916-package-manager-broker-coverage-and-state.md`).
- [ ] Repo-scoped GitHub account selection live acceptance: multiple GitHub accounts on one host, repo-mapped credential wins without touching global `gh auth` active account (from `issues/done/20260916-repo-scoped-github-account-selection.md`).
- [ ] OpenCode `run`/`resume` CLI rebuilt-runtime canary including `delegate --backend opencode --session` resume preflight on real session state (from `issues/done/20260922-opencode-run-cli-compatibility.md`).
- [ ] Server-backend live parity: `codex exec` vs `codex app-server` and `opencode run` vs `opencode serve` task APIs — structured report, usage, observed model/effort, permission denial, interrupt, orphan-free (from `issues/open/20260922-agent-server-backends-cli-deprecation.md`, Phase 2). Partial evidence 2026-09-23 on Ubuntu VM (opencode 1.18.32, codex-cli 0.156.1, main worktree + timeout/messageID fixes): server-side wiring verified end-to-end through real MCP stdio + approval console — `codex_status`/`opencode_status` return compatible:true with real model/provider lists and clean child shutdown; `codex_task_start` → thread/start + turn/start accepted and `codex_task_get` reconciles terminal state with bounded evidence; `opencode_task_start` → session create + prompt_async accepted and `opencode_task_get` reconciles to `retryable_failed` with usage fields populated. Model execution on both backends is credential-blocked on this host (codex turn `401 Unauthorized` from api.openai.com; opencode model call `APIError`, no provider auth), so structured-report/usage-success/permission-denial/interrupt parity items remain unproven. Orphan-free shutdown observed for status probes (no lingering serve/app-server children).

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

## 2026-09-22 salvaged completion evidence

Imported from `docs/evaluations/completion-20260915-salvaged-evidence.md` (the distilled, labeled copy of the deleted completion branches' live evidence):

- The OpenCode provider-entitlement row's 2026-09-15 attempt detail is now recorded: `opencode-go/deepseek-v4-flash` requires explicit regional opt-in, `opencode/gpt-5.6-luna`, `opencode/gpt-5.6-sol`, `opencode-go/mimo-v2.5`, `opencode-go/mimo-v2.5-pro` reported no payment method, and `opencode/mimo-v2.5-free` reported disabled; deterministic adapter suite 73/73 and bounded diagnostics passed. Unchanged open limitation.
- The nested-user-namespace Linux limitation and macOS Vite+ Codex acceptance are recorded under Developer broker / installed runtime above.
- The Cloudflare account read-only probe results are recorded under Cloudflare Worker / Durable Object gateway above.
- Open operational follow-ups with no matrix row: a credential visible in another process's command line during the 2026-09-15 ingress recovery was never rotated (explicitly noted, value never recorded), and the wedged-supervisor control-socket incident is preserved under `issues/open/20260916-upgrade-process-group-friction.md`.
