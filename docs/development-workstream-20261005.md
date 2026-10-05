# Temote MCP / Fabric integration workstream (2026-10-05)

This is an execution checklist for the current `codex/20261005-complete-issues-fabric` integration branch. It tracks issue preparation and the subsequent implementation; executed integration gates and deployment transitions are recorded in [the dated evaluation](evaluations/integration-20261006.md). Use one integration PR after the dependent packets are implemented and reviewed. Parents in `issues/open/` remain open until their children and parent acceptance are complete.

## Current acceptance state

All 31 source issues and 24 implementation children were normalized. Of the
39 implementable packets, 25 are complete against repository fixtures and
contract review, and 14 retain unchecked live/dependency acceptance in
`issues/doing/`. The 16 parent/open issues remain open. Each packet links its
own dated evidence and distinguishes fixture PASS from external NOT RUN.

The final live local→MCP canary exposed a runtime-owner routing defect.
Its delegated supervisor/evidence relay repair and actual both-direction
acceptance passed, including continuation and replay. Fabric
is deployed through `cf` at `temote.f12o.com`, with original DO/D1 identities
and Access denial verified. New Host binaries are installed; production
handoff is blocked by `mbt` restart context. The old Fabric Link remains
running pending authorized 1Password MCP runtime injection. Unavailable
provider, Linux, callback and external gh-git gates are still explicit. An actual managed-source canary was blocked before fetch by child network approval; no repository/workspace marker was created. Its automatic model-selection defect was repaired without widening that approval boundary. A fresh post-repair canary selected the normal default, initialized only an isolated bare store/failed marker, and remained blocked before workspace creation by DNS/network approval. Physical allocation and environment acceptance remain NOT RUN.

## Fixed cross-cutting contract

- Keep Host task/execution authority, full session-instance ownership, canonical scope, bounded evidence, typed operations and mandatory operation receipts. A4 execution/verification/delivery separation was merged in PR #93; verification and delivery do not become PASS merely because execution completed.
- S0a's initial typed source implementation merged in PR #86, while strengthened grammar/entry-point conformance remains in [`S0a conformance`](../issues/done/20260929-s0a-contract-conformance.md). Wire source provisioning only after that gate.
- Managed `session_start` accepts exactly one of repository `source` or existing named-root `path`; the managed form requires caller-supplied `operation_id`. `jj` auto reports unsupported rather than silently using Git. Existing report compatibility profiles remain distinct.
- Keep current `temote-mcp`, `TEMOTE_MCP_`, Gateway command and endpoint compatibility while introducing `temote` and Temote Fabric. Add OpenAI extension metadata to the current Fabric protocol adapter; SDK replacement is outside this workstream.
- Event V1 is `job.state.changed` and `session.state.changed`, webhook only, `cursor: null`. Default TTL is 24 hours, finite maximum 7 days, `ttlMs: null` grants finite 24 hours, and key rotation overlaps 5 minutes. Fabric owns subscription/outbox state; a dedicated Host sender receives egress through an Access-protected Tunnel, pins a validated public IP with original-host TLS, and rejects redirects. Delivery waits while Host is offline until expiry.

## Dependency and execution checklist

| Order | Scope and existing baseline | Packet / remaining work | Completion evidence |
| --- | --- | --- | --- |
| 1 | A4 merged PR #93; S0a initial PR #86 | [`S0a conformance`](../issues/done/20260929-s0a-contract-conformance.md), [`A4 parent`](../issues/open/20260925-a4-task-state-separation.md); preserve separate execution/verification/delivery states | Grammar and checked entry tests; record A4 host/CI gates separately |
| 2 | Existing task stores and app-server/serve backends | [`A core dispatch`](../issues/done/20261005-core-backend-dispatch.md), [`local task frontend/reconciliation`](../issues/done/20261005-local-task-frontend-reconciliation.md), [`Codex continuation`](../issues/done/20260927-codex-task-conversation-continuation.md), [`task revision`](../issues/done/20261001-task-get-semantic-revision-stability.md), [`bounded wait`](../issues/doing/20260927-bounded-wait-for-delegated-tasks.md), [`server parity/CLI cleanup`](../issues/done/20261005-server-backend-legacy-cli-cleanup.md) | Contract snapshots, lost-response, cross-transport, full-instance, default `wait_ms=0` and max `30000` tests |
| 3 | jj-first V3 first slice merged PR #59; V0/V1/V2 complete | [`V3 reconcile/task snapshots`](../issues/doing/20261005-vcs-reconcile-task-snapshots.md), [`named-root reverse resolution`](../issues/done/20260927-named-root-reverse-resolution.md), [`NR2–NR4`](../issues/done/20261005-named-root-identity-enforcement.md) | Accepted receipt reconciliation, durable observation correlation, root admission across starts; jj unsupported never silently falls back |
| 4 | Bare-store and session-source design parent | [`RepositoryStore S1a/F`](../issues/doing/20261005-repository-store-idempotent-ensure.md), [`S2 allocation`](../issues/doing/20261005-managed-workspace-allocation.md), [`S3a start`](../issues/doing/20261005-managed-session-source-start.md), [`OpenCode preflight`](../issues/done/20260927-opencode-task-preflight-capability-blockers.md), [`OpenCode scoped command`](../issues/doing/20261005-opencode-scoped-command-workspace.md), [`C0 gh-git identity`](../issues/doing/20261005-gh-git-common-dir-identity.md) | No-local-main, source XOR path, managed operation_id, replay/crash/freshness, isolated writable workspace and explicit external gh-git gate |
| 5 | D1 Change record prepared | [`D1`](../issues/done/20261001-change-record-correlation.md), [`D2 allocation`](../issues/doing/20261005-change-allocation-writer-handoff.md), [`D3 plan`](../issues/done/20261005-change-delivery-planner.md), [`D4/D5 delivery`](../issues/doing/20261005-change-delivery-adapter.md) | Explicit Change graph, single writer, revision-bound verification, remote receipt reconciliation, no automatic merge |
| 6 | Local O1–O4 and Fabric memory baseline implemented | [`prompt ingress`](../issues/done/20261001-agent-prompt-local-ingress-contract.md), [`hook capabilities`](../issues/done/20261005-agent-prompt-hook-capabilities.md), [`correlation/context`](../issues/done/20261005-agent-prompt-correlation-context.md), [`F1 friction`](../issues/done/20261005-friction-candidate-consumer.md), [`F2 publisher`](../issues/doing/20261005-friction-authorized-publisher.md) | Unsupported hook coverage explicit; no invented direct prompts; separate checkpoints and publication authorization |
| 7 | Current report shapes and native surfaces differ | [`S1 report contract`](../issues/done/20260927-common-task-report-schema.md), [`native capability adapters`](../issues/done/20261005-native-report-capability-adapters.md), [`raw-result bound`](../issues/done/20260927-bound-task-record-with-raw-result.md) | Preserve Delegation and TaskReport profiles; record unsupported native surfaces and honest report_source |
| 8 | Child-runtime recovery and CI defects remain | [`LC1`](../issues/done/20261001-agent-child-watcher-tristate-liveness.md), [`LC2`](../issues/done/20261001-agent-child-cleanup-retry-and-provider-aggregation.md), [`LC3`](../issues/done/20261001-agent-child-durable-task-recovery.md), [`CI1`](../issues/doing/20261001-codex-restart-drain-broken-pipe.md), [`dogfood friction`](../issues/doing/20261005-dogfood-supervisor-metadata-and-link-path.md) | Unknown liveness does not close child; retained task recovery; stale metadata and Fabric Link PATH tested as separate defects |
| 9 | Workspace ready state and product migration | [`environment preparation`](../issues/doing/20261005-environment-preparation.md), [`Fabric naming/deployment`](../issues/doing/20261005-fabric-naming-deployment-migration.md) | Scoped adapters, compatibility aliases, route/DO/D1 coexistence and rollback |
| 10 | PR #90 read-only MCP App baseline | [`Fabric extension metadata/interactions`](../issues/done/20261005-fabric-extension-metadata-interactions.md), [`Events E1`](../issues/done/20261005-fabric-events-subscription-store.md), [`Events E2`](../issues/done/20261005-fabric-events-host-sender.md) | Modern/legacy protocol parity, authorized mentions, durable subscription/outbox, pinned-IP HTTPS sender, webhook retries |
| 11 | Integration and deployment | One integration PR; then update Hosts and deploy with `cf` CLI to `temote.f12o.com` after applicable available-environment preflight | Record actual SHA, command class, deployment target, Host health, rollback readiness, and non-secret evidence. See the dated evaluation for actual deployment versions and unavailable Host gates. |

## Open parent phase inventory

| Open parent | Remaining scope |
| --- | --- |
| [Live matrix](../issues/open/20260908-live-acceptance-matrix.md) | Credentialed Cloudflare/ChatGPT, physical multi-host, provider, installed-runtime and Host-only acceptance; each unavailable row stays NOT RUN. |
| [Server backends](../issues/open/20260922-agent-server-backends-cli-deprecation.md) | Credentialed Codex/OpenCode parity and staged one-shot CLI cleanup; existing server implementations remain credited. |
| [Development harness](../issues/open/20260924-temote-development-harness-restructure.md) | A core dispatch; B local frontend; R cross-transport reconciliation; C0 gh-git identity; C workspace allocation; D environment; E delivery; F bare store/no-local-main; G compatible `temote` rename. |
| [A4](../issues/open/20260925-a4-task-state-separation.md) | PR #93 merged; conformance and host/CI evidence, including verification and delivery state boundaries, remain to be recorded. |
| [Observation/context](../issues/open/20260925-observation-context-memory-plane.md) | Direct-agent prompt ingress/correlation/coverage and remote Fabric qualification; O1–O4 local work stays credited. |
| [VCS transaction](../issues/open/20260925-vcs-transaction-jj-first.md) | V3 reconcile, durable VCS observation, Task/Execution correlation, release, automatic task snapshots; V4 delivery and unsupported jj capability gates. |
| [Named root](../issues/open/20260926-named-root-workspace-identity.md) | NR1 reverse resolution, NR2 logical metadata, NR3 all entry points, NR4 user guidance/migration. |
| [Friction worker](../issues/open/20260926-observation-friction-worker.md) | F1 scoped candidate consumer and F2 authorized publication/outbox; not the already implemented memory extractor. |
| [Change graph](../issues/open/20260926-task-change-orchestration-stacked-pr.md) | D1 durable correlation, D2 writer allocation, D3 deterministic plan, D4 GitHub/gh-stack adapter, D5 lifecycle and delivered-revision binding. |
| [Fabric boundary](../issues/open/20260926-temote-fabric-product-boundary.md) | FBR2 user-facing commands, FBR3 deployment identity/DO migration, FBR4 source rename after compatibility proof; FBR1 is complete. |
| [Native reports](../issues/open/20260927-native-structured-output-agent-backends.md) | S1 shared contract with distinct profiles, then observed per-backend native capability/unsupported results and report_source. |
| [OpenCode checkout](../issues/open/20260927-opencode-checkout-command-execution-capability.md) | OC1 blocker preflight plus OC2 scoped checkout/command capability; existing V2 shell deny is not simply removed. |
| [Session first](../issues/open/20260929-session-first-managed-provisioning.md) | S0a conformance, S1a store/receipts, S2 allocation/jj binding, S3a source XOR path public start; later verification-bound delivery ref. |
| [Prompt ingress](../issues/open/20261001-agent-prompt-observation-ingress.md) | P0/P1 local ingress, P2/P3 verified hooks or explicit gap, P4 correlation, P5 context coverage, P6 sanitization, P7 live cross-agent gate. |
| [MCP extensions](../issues/open/20261001-fabric-openai-mcp-extensions.md) | Optional metadata, mentions and interaction UI on current adapter; PR #90 read-only app remains baseline. |
| [MCP Events](../issues/open/20261005-fabric-mcp-events.md) | E1 catalog/subscriptions/outbox and E2 constrained Host HTTPS sender, filtering, signing, retries and live ChatGPT gate. |

The OpenCode task checkout, native reports, prompt hooks and external provider rows have explicit unsupported/unknown states until their real capability is observed. Parent issues remain open where their live/provider acceptance is outstanding; packet implementation and executed gates are credited in the dated evaluation.

## Validation and live matrix

Implementation packets run focused tests plus `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, `(cd fabric && npm test)` when fabric/shared protocol changes, and `git diff --check`. Review source against each packet's acceptance, then create the single integration PR. Issue normalization passed the common validator; implementation gates are recorded separately in the dated evaluation.

The [`live acceptance matrix`](../issues/open/20260908-live-acceptance-matrix.md) owns external credentials, ChatGPT Work, Cloudflare Access, physical multi-host and provider gates. Unavailable gates stay **NOT RUN** and unchecked. They do not block available-environment host update and `cf` deployment, but deployment itself must not be reported as passed before executed. Record every executed gate with date, tested SHA, provider/host, non-secret evidence and result.

## Remaining capability evidence

- Probe native structured-report mechanisms and direct user-prompt hooks for installed Codex, OpenCode and Devin versions. Unsupported/unknown is an explicit capability or coverage result, not an invented integration.
- Validate submodules, Git LFS and required Git hooks for jj repositories on suitable hosts; retain a capability-gated unsupported result until observed.
- Measure the bounded task wait and provider parity on available live backends. Entitlement-dependent runs remain NOT RUN where no authorized provider is available.
- Verify actual Access/Tunnel, callback sender, live ChatGPT Events and `temote.f12o.com` route in the available deployment environment when implementation reaches that gate.

## Normalized 31 source issue paths

### `issues/open/`

- [`issues/open/20260908-live-acceptance-matrix.md`](../issues/open/20260908-live-acceptance-matrix.md)
- [`issues/open/20260922-agent-server-backends-cli-deprecation.md`](../issues/open/20260922-agent-server-backends-cli-deprecation.md)
- [`issues/open/20260924-temote-development-harness-restructure.md`](../issues/open/20260924-temote-development-harness-restructure.md)
- [`issues/open/20260925-a4-task-state-separation.md`](../issues/open/20260925-a4-task-state-separation.md)
- [`issues/open/20260925-observation-context-memory-plane.md`](../issues/open/20260925-observation-context-memory-plane.md)
- [`issues/open/20260925-vcs-transaction-jj-first.md`](../issues/open/20260925-vcs-transaction-jj-first.md)
- [`issues/open/20260926-named-root-workspace-identity.md`](../issues/open/20260926-named-root-workspace-identity.md)
- [`issues/open/20260926-observation-friction-worker.md`](../issues/open/20260926-observation-friction-worker.md)
- [`issues/open/20260926-task-change-orchestration-stacked-pr.md`](../issues/open/20260926-task-change-orchestration-stacked-pr.md)
- [`issues/open/20260926-temote-fabric-product-boundary.md`](../issues/open/20260926-temote-fabric-product-boundary.md)
- [`issues/open/20260927-native-structured-output-agent-backends.md`](../issues/open/20260927-native-structured-output-agent-backends.md)
- [`issues/open/20260927-opencode-checkout-command-execution-capability.md`](../issues/open/20260927-opencode-checkout-command-execution-capability.md)
- [`issues/open/20260929-session-first-managed-provisioning.md`](../issues/open/20260929-session-first-managed-provisioning.md)
- [`issues/open/20261001-agent-prompt-observation-ingress.md`](../issues/open/20261001-agent-prompt-observation-ingress.md)
- [`issues/open/20261001-fabric-openai-mcp-extensions.md`](../issues/open/20261001-fabric-openai-mcp-extensions.md)
- [`issues/open/20261005-fabric-mcp-events.md`](../issues/open/20261005-fabric-mcp-events.md)

### Original polished source packets (current paths)

- [`issues/done/20260927-bound-task-record-with-raw-result.md`](../issues/done/20260927-bound-task-record-with-raw-result.md)
- [`issues/doing/20260927-bounded-wait-for-delegated-tasks.md`](../issues/doing/20260927-bounded-wait-for-delegated-tasks.md)
- [`issues/done/20260927-codex-task-conversation-continuation.md`](../issues/done/20260927-codex-task-conversation-continuation.md)
- [`issues/done/20260927-common-task-report-schema.md`](../issues/done/20260927-common-task-report-schema.md)
- [`issues/done/20260927-named-root-reverse-resolution.md`](../issues/done/20260927-named-root-reverse-resolution.md)
- [`issues/done/20260927-opencode-task-preflight-capability-blockers.md`](../issues/done/20260927-opencode-task-preflight-capability-blockers.md)
- [`issues/done/20260929-s0a-contract-conformance.md`](../issues/done/20260929-s0a-contract-conformance.md)
- [`issues/done/20260929-s0a-typed-session-source-contract.md`](../issues/done/20260929-s0a-typed-session-source-contract.md)
- [`issues/done/20261001-agent-child-cleanup-retry-and-provider-aggregation.md`](../issues/done/20261001-agent-child-cleanup-retry-and-provider-aggregation.md)
- [`issues/done/20261001-agent-child-durable-task-recovery.md`](../issues/done/20261001-agent-child-durable-task-recovery.md)
- [`issues/done/20261001-agent-child-watcher-tristate-liveness.md`](../issues/done/20261001-agent-child-watcher-tristate-liveness.md)
- [`issues/done/20261001-agent-prompt-local-ingress-contract.md`](../issues/done/20261001-agent-prompt-local-ingress-contract.md)
- [`issues/done/20261001-change-record-correlation.md`](../issues/done/20261001-change-record-correlation.md)
- [`issues/doing/20261001-codex-restart-drain-broken-pipe.md`](../issues/doing/20261001-codex-restart-drain-broken-pipe.md)
- [`issues/done/20261001-task-get-semantic-revision-stability.md`](../issues/done/20261001-task-get-semantic-revision-stability.md)
