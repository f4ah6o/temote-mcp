# Open Issue Implementation Report

Date: 2026-09-12

## Issues addressed

The selected issue slice in this pass is the repository-local deployment preflight from `issues/open/20260911-gateway-deployment-target.md` (Slice B). It follows the already-landed documentation slice and does not perform Cloudflare mutation or require credentials. The open issues were inspected against the current source and their implementation notes:

- `issues/open/20260911-session-forget-stale-metadata.md`: implemented, including supervisor serialization, stale-artifact cleanup, liveness refusal, and filesystem-safety tests.
- `issues/open/20260910-developer-execution-broker.md`: implemented; `dev_tool_run` classification, Cargo/Vite+ execution profiles, tests, and docs are present.
- `issues/open/20260911-default-agent-permission-mode.md`: implemented; permission defaults, centralized approval policy, lifecycle persistence, tests, and docs are present.
- `issues/open/20260911-local-agent-vp-installed-codex-runtime.md`: bounded Vite+ launcher dependency closure is implemented; verification is the remaining issue work.
- `issues/open/20260911-gateway-deployment-target.md`: Slice A documentation/checks and Slice B credential-free target preflight are implemented. Cloudflare deployment verification remains live-only.
- `issues/open/20260911-gateway-doctor-readiness.md`: the recommended Slice A local staged diagnostics are implemented. Remote endpoint, Access, registration, and session checks remain intentionally unimplemented.
- `issues/open/20260910-opencode-delegation-backend.md`: backend extraction, one-shot OpenCode execution, diagnostics, explicit resume preflight, binary override, and report hardening are implemented. Persistent server/session lifecycle, fork, and attach remain deferred.
- `issues/open/20260908-07-client-safe-upgrade-reconnect.md`: only the explicitly ordered durable transaction-storage slice is implemented; remote coordinator/reconnect work remains a separate, substantial feature.
- `issues/open/20260908-08-codex-delegation-dogfood-and-app-server.md`: implementation and fake transport are present; real app-server dogfood and comparative measurement require an appropriately configured host.
- `issues/open/20260908-live-acceptance-matrix.md`: this is credential- and deployment-dependent tracking, not a repository-local implementation target.

## Files changed

- `gateway/scripts/deployment-preflight.mjs`
- `gateway/test/deployment-preflight.test.mjs`
- `gateway/package.json`
- `docs/gateway.md`
- `docs/gateway.ja.md`
- `issues/open/20260911-gateway-deployment-target.md`
- `docs/opencode-implementation-report.md` (this report)

The preflight reads local Wrangler configuration and explicit operator target arguments only. It rejects missing or non-false `workers_dev`, distinguishes missing and mismatched targets, and returns `remote_unknown` rather than claiming Cloudflare readiness.

## Design decisions

- Do not reimplement completed slices or mark external acceptance as complete without evidence.
- Do not add remote gateway probing, upgrade orchestration, or persistent OpenCode lifecycle behavior without the required protocol and security review boundaries.
- Keep deployment preflight read-only and credential-free; remote verification remains an explicit live acceptance step.
- Preserve existing user work; no reset, checkout, cleanup, or commit operation was performed.

## Checks

OpenCode did not have shell execution in its coding environment, so the parent Temote session reran the repository-local checks after implementation:

```text
node --test test/deployment-preflight.test.mjs  # PASS: 5/5
(cd gateway && npm test)                        # PASS: 65/65
git diff --check                                # PASS
```

The Slice B change is isolated to gateway JavaScript/docs/issue tracking, so no Rust source changed in this pass. The broader Rust fmt/test/clippy/no-default-features gates were not rerun here.

## Remaining blockers

- Cloudflare route/domain, Access, Worker, host-agent, and multi-host acceptance require deployment credentials and live hosts.
- Gateway deployment preflight now covers local target selection; remote route/domain verification remains live-only.
- Gateway doctor Slices B-D require a reviewed read-only remote status contract.
- Client-safe remote upgrade requires coordinator ownership, response-flush commit signaling, boot-generation identity, reconnect status, and process-boundary E2E coverage.
- OpenCode persistent lifecycle/app-server behavior remains explicitly deferred after the bounded one-shot/resume implementation.
- Codex app-server live dogfood and comparative measurement require a configured Codex installation and supported credentials.

## Git status summary

- Branch: `main`
- HEAD: `5883599691122ecfe5245925cf77ccc1b963ca32`
- Parent verification confirmed the Slice B files are uncommitted in the current worktree. Pre-existing `.worktrees/` remains untracked and was not modified or discarded.
