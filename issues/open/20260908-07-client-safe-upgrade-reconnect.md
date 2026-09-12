# Client-safe Temote upgrade with durable reconnect verification

Date: 2026-09-08

## Implementation status (2026-09-12)

The repository-local boot identity slice is implemented: each Temote process now
generates one UUID-shaped, non-secret `boot_generation`, and the existing
`/healthz` response reports it together with the effective `host_id`. This is
an identity primitive for later reconnect verification only; remote upgrade
tools, response-flush commit barriers, coordinator ownership, and live
reconnect acceptance remain unimplemented.

## Implementation status (2026-09-12, coordinator-safe transaction primitives)

The repository-local coordinator/observation core is implemented on top of the
step-1 schema:

- `upgrade_transaction` exposes a bounded, non-secret `UpgradeTransactionStatus`
  view (`terminal` flag, versions, host, timestamps, restart/reconnect flags,
  failure summary) for a reconnect-safe status response.
- Deterministic selection helpers (`recent_transaction`,
  `latest_completed_transaction`, `active_transactions`) and
  `classify_apply` implement retry idempotency (`ExistingActive`),
  conflicting-active-target rejection (`ConflictActive`), and
  completed-target no-op (`AlreadyCompleted`).
- `transaction_lock_is_held` probes the existing owner lock read-only without
  creating files, and `incomplete_upgrade_transactions` classifies non-terminal
  transactions with no live owner as stale/incomplete rather than success.
- `/healthz` now reports `last_upgrade_transaction` (bounded, non-secret,
  best-effort) in addition to `host_id` and `boot_generation`.

Still unimplemented: the one-shot `upgrade-coordinator` process, the transport
response-flush commit barrier, the remote `upgrade_preflight` / `upgrade_apply`
/ `upgrade_status` tools, and the deliberate-disconnect process E2E on macOS and
Linux.

## Implementation status (2026-09-13, reconnect identity metadata)

The MCP handshake now exposes the same bounded, non-secret process identity that
`/healthz` already reports, so a reconnecting MCP client can verify the intended
host, version, and boot generation without an out-of-band health probe:

- `initialize`, `ping`, and `server/discover` include
  `_meta["io.temote/processIdentity"] = {host_id, version, boot_generation}`.
- `host_id` is the validated stable host identity (OS-hostname fallback),
  `version` is the running package version, and `boot_generation` is the existing
  per-process UUID. All three are non-secret.
- `modernize_result` now merges the modern `serverInfo` entry into an existing
  `_meta` object instead of replacing it, so modern `initialize`/`ping` retain the
  identity field alongside the existing server metadata.
- This completes the "health/initialize/ping identity metadata" item of the endpoint
  generation-identity contract. The coordinator process, response-flush commit
  barrier, remote upgrade tools, and both-platform deliberate-disconnect E2E remain.

## Implementation status (2026-09-13, coordinator-safe transition primitives)

The next repository-local slice adds the coordinator-safe primitives the one-shot
coordinator will drive:

- `UpgradeTransactionState::can_transition_to` plus an enforcing `set_state`
  make coordinator progress monotonic (`prepared -> committed ->
  supervisor_handoff -> sessions_verifying -> ingress_restarting ->
  endpoint_verifying -> plugin_reconciling -> completed`) while still allowing
  optional stages to be skipped. Terminal states never transition again.
- `UpgradeTransaction::mark_failed` / `mark_rolled_back` record deterministic
  terminal outcomes with the same bounded, NUL-free failure-summary rules as
  persisted transactions.
- `UpgradeTransaction::coordinator_phases` derives the ordered post-commit
  phases from `supervisor_handoff_required` / `ingress_restart_required`.
- `UpgradeCommitBarrier` / `UpgradeCommitWaiter` replace timing-based ordering:
  the transport commits only after the `accepted` response is written and
  flushed, aborts on delivery failure, and a dropped sender fails closed to
  `Aborted`. Concurrent cloned senders are serialized inside the watch update,
  so exactly one `commit`/`abort` decision can win. The waiter resolves without
  any wall-clock sleep.

Remaining: the one-shot upgrade-coordinator process, the response-flush barrier
wired into the HTTP/MCP transport, the remote upgrade tools, and both-platform
deliberate-disconnect E2E. The new primitives are not yet wired into the local
`session_control` upgrade path, so no runtime behavior changes in this slice.

## Implementation status (2026-09-13, coordinator state machine)

The repository-local coordinator state machine is implemented on top of the durable
transaction schema, still without an OS process wrapper, transport wiring, or remote
tools:

- `UpgradeCoordinatorExecutor` plus `run_upgrade_coordinator` drive one `prepared`
  transaction through `coordinator_phases()`.
- The driver holds the exclusive transaction lock for the whole run, so a second live
  coordinator fails closed. It refuses a transaction that is not `prepared`.
- It awaits the existing response-flush `UpgradeCommitBarrier` before any destructive
  phase. An aborted or lost transport records a terminal `failed` transaction and runs
  no phase (no timing heuristic).
- Each phase is persisted before the next begins. A phase may report `Continue`,
  `Rollback` (terminal `rolled_back`), or an error (terminal `failed` with a bounded
  non-secret summary), so a crash or failed phase leaves a deterministic non-success
  state that `incomplete_upgrade_transactions()` reports after reconnect.
- Deterministic coverage: lost-transport abort with zero phases, ordered completion
  of every required phase, rollback short-circuit, bounded phase failure,
  second-owner / non-prepared refusal, and concurrent commit/abort one-shot
  enforcement.

Still unimplemented: the one-shot `upgrade-coordinator` OS process and its concrete
executor, transport commit wiring, remote `upgrade_preflight` / `upgrade_apply` /
`upgrade_status` tools, and the deliberate-disconnect process E2E.

## Implementation status (2026-09-11)

Suggested implementation order step 1 landed on main: `src/upgrade_transaction.rs` provides the durable transaction schema (`UpgradeTransaction`, `UpgradeTransactionState` with prepared/committed/…/completed/failed/rolled_back), owner-only bounded atomic storage under `<state>/upgrade-transactions/<uuid>.json`, strict canonical UUID path validation, symlink/public-mode/oversize rejection on read, an exclusive `flock`-based per-transaction lock with automatic stale-owner release, bounded transaction listing, terminal-state locking, and secret-free schema tests. The remote tools, coordinator, response-flush barrier, and reconnect contract remain unimplemented.

## Background

Temote already has a strong local upgrade path from `issues/open/20260902-zero-downtime-supervisor-upgrade.md`:

- the lifecycle supervisor validates a target binary before destructive action;
- active sessions are fenced, drained, restored, and socket-probed;
- the supervisor changes implementation with same-PID `exec` handoff;
- direct ingress restart state is persisted without credential values;
- a required direct-ingress restart is performed by the outer `temote-mcp upgrade` CLI and verified with a bounded `/healthz` probe;
- deterministic failure reports exist for post-`exec` restore failures.

That operator-driven flow assumes the process running `temote-mcp upgrade` is outside the ingress that may be restarted.

A remote MCP client introduces a different ownership problem. If a client invokes an upgrade through the same Temote ingress that must be replaced, the server can accept the request and then terminate the connection as part of its own restart. The current outer CLI is no longer available as an independent coordinator to prove that the replacement ingress became healthy and that the client reconnected to the intended new generation.

The missing property is therefore not another supervisor handoff. It is a durable, externally observable upgrade transaction whose execution survives loss of the initiating ingress connection.

## Problem statement

Temote should allow an authenticated MCP client to request application of an already-installed, locally trusted Temote binary and recover safely across the intentional transport interruption caused by ingress replacement.

The desired contract is:

> Once Temote reports that a remote upgrade transaction has been accepted, the remainder of the upgrade is owned by a process boundary that is not destroyed with the initiating ingress. The replacement endpoint must prove target identity and transaction completion after reconnection before the client treats the upgrade as successful.

This must preserve existing security boundaries. A remote caller must not gain arbitrary executable selection, package installation, shell execution, credential persistence, yolo creation, or control over unrelated host processes.

## Relationship to the existing upgrade issue

This issue extends, rather than replaces, `20260902-zero-downtime-supervisor-upgrade.md`.

Reuse the existing mechanisms wherever possible:

- `SupervisorUpgradePlan` and same-PID supervisor `exec` handoff;
- target `supervisor --capabilities` validation;
- restart-context key-only persistence;
- session drain/restore/socket verification;
- direct-ingress durable runtime metadata and restart recipe validation;
- direct-ingress `/healthz` target-version verification;
- Codex plugin reconciliation;
- existing rollback and deterministic failure reporting.

The new work is primarily transaction ownership, response/commit ordering, and reconnect verification.

## Threat model and safety invariants

The remote upgrade API must remain narrower than the local CLI.

Required invariants:

- remote callers cannot provide an executable path;
- remote callers cannot provide a package URL, command, argv, shell fragment, installer, or arbitrary service definition;
- the candidate executable is selected only from Temote-owned local installation state;
- the candidate still passes the same capability/version/protocol/schema validation as local `temote-mcp upgrade`;
- no credential value is written to transaction state, restore plans, failure reports, logs, MCP results, or diagnostics;
- public HTTP still cannot create yolo sessions or bypass local approval policy;
- accepted upgrade state must be owner-only, bounded, regular-file data and reject symlinks/untrusted ownership just like other lifecycle state;
- a stale or forged transaction file must not authorize execution of an arbitrary binary;
- authentication and authorization after reconnect are re-established normally; an upgrade transaction is not an authentication bearer token.

## Proposed remote MCP surface

Expose a deliberately small lifecycle API, for example:

```text
upgrade_preflight()
upgrade_apply(expected_version?)
upgrade_status(transaction_id)
```

Names may change to match project conventions, but the responsibilities should remain separate.

### `upgrade_preflight`

Read-only. Returns the same core facts as local `upgrade --dry-run`, narrowed for remote consumption:

- running supervisor version;
- installed/canonical candidate version;
- compatibility result;
- whether supervisor handoff is required;
- blocked-session count and non-secret blocker reasons;
- direct-ingress action required;
- whether a reconnect interruption is expected;
- whether plugin reconciliation/client restart remains after the transport comes back.

Do not return executable paths unless there is a clear diagnostic reason; the remote caller does not need them to authorize execution.

### `upgrade_apply`

Mutating and explicit.

It should:

1. recompute all preflight checks instead of trusting an earlier preview;
2. select the canonical locally installed Temote candidate;
3. validate target capabilities/version/protocol/schema;
4. create an owner-only durable transaction record with a random opaque ID;
5. spawn or hand off to an upgrade coordinator that is outside the direct-ingress lifetime;
6. return `accepted` plus the transaction ID and target version;
7. only after the response is confirmed flushed to the client, signal the coordinator that destructive transition may begin.

Optional `expected_version` is an optimistic-concurrency guard only. If supplied and the locally selected target changed, fail before mutation.

### `upgrade_status`

Read-only and reconnect-safe.

Given a transaction ID, return a bounded status such as:

```text
prepared
committed
supervisor_handoff
sessions_verifying
ingress_restarting
endpoint_verifying
plugin_reconciling
completed
failed
rolled_back
```

Include only non-secret diagnostics:

- source version;
- target version;
- transaction state;
- created/updated timestamps;
- whether reconnect is/was required;
- final verified server generation/version;
- counts or IDs of restored/unrestored sessions when already allowed by existing diagnostics;
- deterministic failure classification and safe error text.

Transaction IDs should be opaque, random, bounded, and validated before filesystem use.

## Upgrade coordinator

### Requirement

The coordinator must not share the lifetime of the ingress being replaced.

A simple acceptable first design is a one-shot local process launched from the validated Temote binary before the initiating ingress is stopped. It receives only the minimum non-secret durable state needed to continue the transaction and reacquires any approved restart context through the same existing mechanisms.

Possible CLI-internal form:

```text
temote-mcp upgrade-coordinator --transaction <id>
```

This command should be internal/hidden if possible and must not accept arbitrary executable or command input from the remote API.

### Coordinator responsibilities

After commit authorization, it should own the remainder of the transaction:

1. re-read and validate the transaction state;
2. revalidate candidate identity immediately before mutation;
3. apply/verify supervisor handoff using existing upgrade primitives;
4. verify the intended active-session set;
5. restart direct ingress only if required;
6. wait for the replacement origin to report healthy and target version;
7. verify a new boot/generation identity rather than accepting a stale old process;
8. reconcile plugin state where applicable;
9. atomically record `completed`, or deterministic `failed` / `rolled_back` state.

The initiating request handler must not be responsible for steps 3-9.

## Response-flush commit barrier

Do not implement this as `sleep(250ms)` or another timing heuristic.

The destructive phase must not begin until the MCP/HTTP response containing `accepted` and the transaction ID has actually been handed through the response-writing path successfully.

Preferred model:

```text
request handler
  -> prepare transaction
  -> start coordinator in PREPARED state
  -> produce MCP response
transport
  -> write/flush response
  -> commit callback / oneshot signal
coordinator
  -> PREPARED -> COMMITTED
  -> destructive transition may begin
```

If response serialization/write/flush fails, the coordinator remains uncommitted and must expire/abort without stopping the active ingress.

The exact hook depends on the HTTP/MCP stack, but the invariant must be testable without wall-clock sleeps.

## Endpoint generation identity

A health check returning HTTP 200 is insufficient proof that the client reached the intended replacement generation.

Expose a non-secret stable host identity plus per-process/per-boot generation identity in the health/initialize/ping path, for example:

```text
host_id
version
boot_generation
last_upgrade_transaction
```

`boot_generation` should change whenever the direct-ingress owner is replaced. It may be a random startup UUID or another collision-resistant non-secret identifier.

After restart, coordinator success requires:

- expected stable host identity;
- target Temote version;
- a boot generation different from the source ingress when a restart was required;
- the expected transaction ID visible as the last applied/active transaction where appropriate;
- existing `/healthz` readiness conditions.

Do not expose secrets through readiness metadata.

## Client reconnect contract

Temote cannot force every MCP client implementation to reconnect a broken transport. The server-side guarantee should therefore be precise:

> After returning `accepted`, Temote keeps durable transaction state and completes or deterministically fails the upgrade independently of the initiating MCP connection. The same configured endpoint becomes ready on the verified target generation when successful, and `upgrade_status(transaction_id)` allows a reconnected client to determine the final result.

Recommended client behavior:

```text
upgrade_apply
  -> accepted(transaction_id, target_version, reconnect_expected=true)
  -> connection may close
  -> reconnect to the same configured endpoint using normal authentication
  -> initialize / ping
  -> verify target version + host/generation identity
  -> upgrade_status(transaction_id)
  -> continue only when completed
```

A transport/client that has no reconnect capability may still lose the live session, but Temote must leave enough durable verified state for a later connection to determine exactly what happened.

## Idempotency and duplicate requests

Remote retries are expected around a deliberate disconnect.

Required behavior:

- `upgrade_status` is idempotent;
- retrying `upgrade_apply` while an active transaction already targets the same candidate should return the existing transaction or a precise `upgrade_in_progress` result rather than start a second coordinator;
- conflicting target/version state fails closed;
- completed same-version state becomes a no-op on a new request unless explicit repair semantics are later designed;
- only one destructive upgrade transaction may own the local runtime at a time.

Use an owner-only lock/lease with deterministic stale-owner recovery rather than relying solely on an in-memory mutex.

## Transaction durability

Persist the minimum state necessary to survive ingress replacement and coordinator/client disconnect.

Suggested shape:

```text
schema
transaction_id
source_version
target_version
host_id
source_boot_generation
state
created_at
updated_at
reconnect_expected
supervisor_handoff_required
ingress_restart_required
safe failure summary / report reference
```

Do not persist:

- credential values;
- bearer/access tokens;
- arbitrary executable paths supplied by clients;
- arbitrary environment values;
- request headers/cookies;
- command/argv/stdout/stderr dumps.

Follow existing bounded-read, owner-only-permission, `O_NOFOLLOW`, atomic-write, and path-containment patterns used by Temote lifecycle state.

## Failure semantics

### Failure before commit

If preflight, response generation, response write, or commit signaling fails:

- do not stop supervisor or ingress;
- mark/expire the prepared transaction safely;
- remote retry is allowed.

### Failure after commit but before ingress stop

Record deterministic failure and leave the still-working ingress available whenever possible.

### Failure after ingress stop

The coordinator owns recovery. It should attempt the existing safe restart/rollback path, persist the outcome, and never report `completed` unless the endpoint generation/version/readiness checks pass.

### Coordinator crash

On startup, Temote should be able to classify non-terminal transaction state as stale/incomplete. A later explicit `upgrade_status` or repair invocation must report that state rather than silently claiming success.

Automatic recovery may be added only if ownership and candidate identity can be revalidated safely.

## Scope of package installation

This issue is about safely applying an already-installed/local trusted Temote binary and surviving the resulting restart.

It does not grant the remote MCP client permission to download or install a new binary from crates.io/GitHub/other network sources.

If remote package acquisition is designed later, it requires a separate issue and explicit trust/signature/source policy.

## Required tests

1. remote preflight is read-only and returns no credential values or arbitrary executable control.
2. `upgrade_apply` creates one owner-only bounded transaction and returns `accepted` before destructive action.
3. simulated response-write/flush failure never sends the commit signal and leaves ingress/supervisor untouched.
4. commit signal causes the coordinator to proceed without relying on a fixed sleep.
5. coordinator remains alive when the initiating HTTP connection is dropped immediately after `accepted`.
6. compatible supervisor handoff reuses the existing same-PID/session-restore guarantees.
7. direct-ingress restart produces a new boot generation, target version, healthy endpoint, and matching host identity.
8. stale healthy origin with the source boot generation is rejected as upgrade success.
9. reconnect + `initialize`/`ping` + `upgrade_status` proves the target transaction completed.
10. duplicate retry during an active transaction does not launch a second destructive coordinator.
11. conflicting transaction/candidate state fails closed.
12. transaction files reject symlinks, unsafe ownership/mode, oversize content, invalid IDs, and out-of-tree paths.
13. transaction/failure persistence contains no captured restart-context values, tokens, auth headers, or cookies.
14. coordinator crash leaves a deterministic non-success transaction state discoverable after reconnect.
15. ingress restart failure records failure/rollback outcome and is never reported as `completed`.
16. same-version completed upgrade is idempotent.
17. Cloudflare, Tailscale, and OpenAI direct-ingress restart recipes retain their existing fail-closed credential behavior.
18. existing local CLI `temote-mcp upgrade` behavior remains compatible and may reuse the coordinator internally without regression.
19. macOS and Linux process-boundary E2E tests deliberately drop the initiating client connection and verify successful later reconnection/status inspection.

## Acceptance criteria

- [ ] an authenticated remote MCP client can request application of the canonical already-installed Temote target without supplying an executable path
- [ ] destructive upgrade work is owned outside the lifetime of the direct ingress being replaced
- [ ] the initiating response is flushed successfully before the destructive transaction is committed
- [ ] no fixed-delay sleep is used as the correctness mechanism for response-before-restart ordering
- [ ] transaction state survives loss of the initiating MCP connection
- [ ] reconnecting to the same endpoint can verify stable host identity, target version, and replacement boot generation
- [ ] `upgrade_status(transaction_id)` deterministically reports terminal success/failure after reconnect
- [ ] successful status requires existing session-restore checks plus replacement endpoint health/identity checks
- [ ] duplicate/retried remote requests cannot create concurrent destructive upgrades
- [ ] transaction persistence follows Temote owner-only, bounded, no-symlink, non-secret state rules
- [ ] no credential value, token, auth header, cookie, arbitrary command, or client-controlled executable path is persisted or executed
- [ ] existing sandbox/yolo/approval/public-HTTP boundaries are unchanged
- [ ] direct-ingress restart remains fail-closed when its restart recipe cannot safely reacquire required credentials
- [ ] local CLI upgrade remains supported and shares primitives rather than growing a divergent implementation
- [ ] process-boundary E2E covers deliberate connection loss and later reconnect on both macOS and Linux
- [ ] English/Japanese operator and Agent Skill documentation clearly state the reconnect contract and the limit that Temote cannot force reconnect behavior in an arbitrary MCP client

## Suggested implementation order

1. Add durable transaction schema, safe storage helpers, locking, and tests.
2. Add startup `boot_generation` plus health/initialize/ping identity metadata.
3. Extract existing local upgrade orchestration into reusable coordinator-safe primitives without changing behavior.
4. Add one-shot upgrade coordinator process and transaction state transitions.
5. Add explicit response-flush/commit barrier in the HTTP/MCP transport path.
6. Add narrow remote preflight/apply/status tools with no executable-path input.
7. Add duplicate/idempotency/stale-transaction handling.
8. Add deliberate-disconnect process E2E on macOS and Linux.
9. Update docs and `skills/temote-mcp/SKILL.md` only after protocol behavior is stable.

## Non-goals

- guaranteeing that every third-party MCP client automatically reconnects;
- keeping one TCP/HTTP connection alive across direct-ingress process replacement;
- remote arbitrary package installation or self-update from the network;
- remote selection of an arbitrary local executable;
- cross-host HA/failover;
- weakening authentication, approval, sandbox, or yolo boundaries;
- persisting credentials to make upgrade recovery easier.
