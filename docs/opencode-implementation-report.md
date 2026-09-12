# Open Issue Implementation Report

Date: 2026-09-12

## Issues addressed

The selected issue slice in this pass is the repository-local deployment preflight from `issues/open/20260911-gateway-deployment-target.md` (Slice B). It follows the already-landed documentation slice and does not perform Cloudflare mutation or require credentials. The open issues were inspected against the current source and their implementation notes:

- `issues/open/20260911-session-forget-stale-metadata.md`: implemented, including supervisor serialization, stale-artifact cleanup, liveness refusal, and filesystem-safety tests.
- `issues/open/20260910-developer-execution-broker.md`: implemented; `dev_tool_run` classification, Cargo/Vite+ execution profiles, tests, and docs are present.
- `issues/open/20260911-default-agent-permission-mode.md`: implemented; permission defaults, centralized approval policy, lifecycle persistence, tests, and docs are present.
- `issues/open/20260911-local-agent-vp-installed-codex-runtime.md`: bounded Vite+ launcher dependency closure is implemented; verification is the remaining issue work.
- `issues/open/20260911-gateway-deployment-target.md`: Slice A documentation/checks and Slice B credential-free target preflight are implemented. Cloudflare deployment verification remains live-only.
- `issues/open/20260911-gateway-doctor-readiness.md`: local staged diagnostics and the read-only remote endpoint, Access, and host-registration checks are implemented. Session availability remains intentionally `not_checked`, and live Cloudflare verification is pending.
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
- Gateway doctor session-availability verification and live Cloudflare route, Access, and lease evidence remain pending; the repository-local read-only status contract is implemented and covered by gateway tests.
- Client-safe remote upgrade requires coordinator ownership, response-flush commit signaling, boot-generation identity, reconnect status, and process-boundary E2E coverage.
- OpenCode persistent lifecycle/app-server behavior remains explicitly deferred after the bounded one-shot/resume implementation.
- Codex app-server live dogfood and comparative measurement require a configured Codex installation and supported credentials.

## Git status summary

- Branch: `main`
- HEAD: `5883599691122ecfe5245925cf77ccc1b963ca32`
- Parent verification confirmed the Slice B files are uncommitted in the current worktree. Pre-existing `.worktrees/` remains untracked and was not modified or discarded.

## Pass 2 — gateway doctor host-registration classification (2026-09-12)

### Issue addressed

`issues/open/20260911-gateway-doctor-readiness.md` (Slice C remainder, repository-local). This pass did not add a remote protocol or perform Cloudflare work; it tightened the classification of the already-implemented read-only `POST /v1/hosts/status` probe.

### Files changed

- `src/doctor.rs`
- `gateway/test/protocol.test.mjs`
- `issues/open/20260911-gateway-doctor-readiness.md`
- `docs/opencode-implementation-report.md` (this report)

### What changed

- Added pure helpers `gateway_health_identity_ok` and `classify_gateway_host_status`; `check_gateway_remote` now delegates endpoint identity and host-registration classification to them.
- Authenticated `404` now distinguishes `not_registered` from `lease_expired` using the worker's bounded `status` field instead of a single generic message.
- `401`/`403` and unexpected statuses now emit an explicit `host_registration=not_checked` result rather than omitting the stage; no failure path maps to `ready`.
- Successful registration detail may include the non-secret active `generation`.
- Added `#[cfg(feature = "network")]` unit tests for identity metadata, registered/mismatched host, `not_registered`, `lease_expired`, unauthorized, and unexpected-status classification.
- Added a gateway Worker protocol test asserting the `/healthz` identity/readiness fields the Rust doctor depends on are present and credential-free.

### What was intentionally not changed

- No new remote protocol, no session-availability probing, no Cloudflare calls, no lease or session mutation.
- `session_availability` remains `not_checked`.
- No upgrade-coordinator or OpenCode persistent-lifecycle work, which remain substantial separate features.

### Checks

This environment exposes no shell/command-execution tool, so `cargo test doctor`, `cargo fmt`, `cargo clippy`, `cargo check`, the gateway `npm test` suite, and `git diff --check` could not be executed here. The change is type-checked only by inspection and follows existing patterns. Verification is required before merge:

```text
cargo test doctor
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
(cd gateway && npm test)
git diff --check
```

### Remaining blockers

- Live Cloudflare route, Access, host-agent lease, and multi-host evidence remain credential/deployment dependent.
- `generation_replaced` is still not separately observable from the read-only status contract.
- Client-safe remote upgrade and OpenCode persistent lifecycle remain unimplemented.

### Git status

No commit or push was performed. Existing uncommitted work was preserved.

## Pass 3 — gateway doctor generation_replaced classification (2026-09-12)

### Issue addressed

`issues/open/20260911-gateway-doctor-readiness.md` (the explicitly recorded `generation_replaced` classification residue). This pass stayed repository-local: no remote protocol change, no Cloudflare call, and no credentials.

### Files changed

- `src/gateway.rs`
- `src/doctor.rs`
- `docs/gateway.md`
- `docs/gateway.ja.md`
- `issues/open/20260911-gateway-doctor-readiness.md`
- `docs/opencode-implementation-report.md` (this report)

### What changed

- The host-level gateway agent now writes a bounded, owner-only, non-secret connection record at `<state>/gateway-agents/<host_id>.json` after a successful `connect`. The record holds only `schema`, `host_id`, `generation`, and `updated_at`, uses atomic `O_NOFOLLOW` create-then-rename with mode `0600`, and is removed when the owning generation ends (RAII drop).
- Added `gateway::read_host_agent_generation`, which reads that record read-only: it never creates the directory, rejects symlinks, non-regular files, files larger than 4 KiB, non-owner-only modes, schema/host-ID mismatches, and zero generation/timestamp.
- `doctor` now compares the authenticated `/v1/hosts/status` `generation` against the local agent generation. A newer remote generation classifies as `generation_replaced` (failed), an older remote generation as `unavailable`, and an exact match as `ready` with the local generation named. No state maps to `ready` on mismatch.
- `docs/gateway.md` and `docs/gateway.ja.md` document the comparison and the `generation_replaced` outcome.
- Added deterministic Rust unit tests: connection-record round-trip and drop removal, owner-only/symlink rejection, and the replacement/matched/older classification cases. Tests make no network calls.

### What was intentionally not changed

- No gateway Worker protocol or `/v1/hosts/status` change; the existing read-only `generation` field is sufficient once the local agent record exists.
- `session_availability` remains intentionally `not_checked`.
- No `sleep`-based or timing-based detection; classification is exact integer comparison.
- No upgrade-coordinator, remote upgrade tool, or OpenCode persistent-lifecycle work.

### Known limitation

- A record left behind by a crashed agent whose gateway lease is still current and unadvanced cannot be distinguished from a live agent by read-only status alone. This is an availability gap, not a false `ready` claim about registration identity.

### Checks

This environment exposes no shell/command-execution tool, so `cargo test`, `cargo fmt --check`, `cargo clippy`, `cargo check --no-default-features --all-targets`, the gateway `npm test` suite, and `git diff --check` could not be executed here. The change is type-checked by inspection only and must be verified before merge:

```text
cargo test doctor
cargo test gateway
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
(cd gateway && npm test)
git diff --check
```

### Remaining blockers

- Live Cloudflare route, Access, host-agent lease, and multi-host evidence remain credential/deployment dependent.
- `session_availability` remains `not_checked` by design.
- Client-safe remote upgrade and OpenCode persistent lifecycle remain unimplemented.

### Git status

No commit or push was performed. All uncommitted work from the previous passes was preserved.

## Pass 4 — client-safe upgrade coordinator/observation core (2026-09-12)

### Issue addressed

`issues/open/20260908-07-client-safe-upgrade-reconnect.md`. This pass implemented the
repository-local coordinator-safe primitives and durable read-only status that the
issue's steps 2-4 and 7 require, without adding a remote protocol, transport barrier,
or any credential-dependent call.

### Files changed

- `src/upgrade_transaction.rs`
- `src/http.rs`
- `issues/open/20260908-07-client-safe-upgrade-reconnect.md`
- `docs/opencode-implementation-report.md` (this report)

### What changed

- Added `UpgradeTransactionStatus` plus `UpgradeTransaction::status()`: a bounded,
  non-secret, reconnect-safe projection (`terminal`, source/target version, host,
  timestamps, reconnect/handoff/ingress flags, failure summary).
- Added deterministic selection helpers `recent_transaction`,
  `latest_completed_transaction`, and `active_transactions`; ties are broken by
  transaction ID so two writes within the same second resolve consistently.
- Added `classify_apply`, which returns `ExistingActive` for an idempotent same-target
  retry, `ConflictActive` for a different active target, `AlreadyCompleted` for a
  completed same-target request, and `StartNew` otherwise. This enforces "only one
  destructive upgrade owns the runtime".
- Added `transaction_lock_is_held`, a read-only owner probe that opens the existing
  lock with `O_NOFOLLOW`, verifies regular file/owner-only mode, and uses
  `flock(LOCK_EX|LOCK_NB)` without creating the lock file.
- Added `incomplete_upgrade_transactions`, which classifies non-terminal transactions
  with no live owner lock as stale/incomplete instead of success.
- `/healthz` now includes `last_upgrade_transaction` via `latest_transaction_id`, a
  best-effort read-only lookup that never creates state directories and reports `null`
  rather than claiming state on any anomaly.
- Added deterministic tests for the status view, recency/tie-break, completed
  selection, non-terminal filtering, apply disposition, and read-only lock-owner
  probing. None of the new tests perform network calls.

### What was intentionally not changed

- No `upgrade-coordinator` process, no response-flush commit barrier, and no remote
  `upgrade_preflight` / `upgrade_apply` / `upgrade_status` tools.
- No credential use, no Cloudflare/ingress state change, and no local CLI `upgrade`
  behavior change.

### Checks

The parent Temote session reran the repository-local checks after implementation:

```text
cargo fmt --all -- --check                              # PASS (repository-wide)
cargo test sandbox::generic_tests::developer_tool_environment_forces_the_sandbox_marker --locked  # PASS
cargo test upgrade_transaction --locked                 # PASS: 16/16
cargo test healthz --locked                             # PASS: 1/1
cargo clippy --all-targets --locked -- -D warnings      # PASS
cargo check --no-default-features --all-targets --locked  # PASS (existing dead_code warnings only)
git diff --check                                        # PASS
```

Repository-wide `cargo fmt --all -- --check` now passes; the earlier
`src/doctor.rs` formatting failure was corrected by rustfmt and is no longer present.

The latest controlled broad-suite attempt was `cargo test --all-targets --all-features
--locked`, run by the parent Temote session under the dev-offline sandbox with `HOME`
redirected into the workspace and `TEMOTE_MCP_SANDBOX=1` forced through Cargo
configuration so upgrade transaction and developer-broker state is writable. Result:
**FAIL** overall — 604 passed, 76 failed, 1 ignored. The representative failures are the
inability to bind `/tmp/temote-mcp-<uid>/*.sock` Unix sockets and loopback/host-IPC
listeners under macOS Seatbelt, which returns `Operation not permitted (os error 1)`;
this affects approvals, codex_app_server, config, http, mcp, session_control,
supervisor, and related integration-style tests. The newly touched
`upgrade_transaction` tests and
`http::tests::healthz_exposes_non_secret_process_identity` passed in that broad run as
well. This run used the existing live developer broker plus Cargo environment overrides,
because the live server has not been replaced with the source-built broker. The remaining
failures are a macOS Seatbelt capability boundary that intentionally denies host-IPC and
loopback network operations; they are not evidence that the broad suite is green, and
developer-tool network/IPC capability was not broadened to make them pass.

### Remaining blockers

- The one-shot coordinator, transport commit barrier, remote tools, and macOS/Linux
  deliberate-disconnect E2E remain unimplemented.
- Live Cloudflare route/Access/lease evidence remains credential/deployment dependent.

### Git status

No commit or push was performed. Existing worktree changes were preserved.

## Pass 5 — upgrade transaction test-cleanup robustness (2026-09-12)

### Issue addressed

A real test-robustness defect surfaced while triaging the dev-offline full-suite
failures: `upgrade_transaction::tests::Fixture::drop` derived its lock-file cleanup
path with `lock_path(...).unwrap()`. `lock_path` calls `ensure_directory()`, so when the
state directory was unavailable the `unwrap()` panicked during unwinding and aborted the
entire test binary instead of letting the original failure be reported.

### Files changed

- `src/upgrade_transaction.rs`
- `docs/opencode-implementation-report.md` (this report)

### What changed

- `Fixture::drop` now attempts `remove_transaction` first and then derives the lock-file
  path from the read-only `upgrade_transaction_directory()` result. It performs no
  `unwrap`, creates no state directories merely to compute the cleanup path, and ignores
  missing artifacts or removal errors.
- Extended the deterministic `transaction_lock_is_exclusive_and_released_on_drop` test to
  remove the transaction and lock artifacts and then drop the fixture explicitly. This
  freezes the non-panicking, best-effort cleanup contract without any global environment
  mutation or new test binary.
- No sandbox, path, network, permission, or release/version metadata was changed.

### Checks

The parent Temote session reran the repository-local checks after this change:

```text
cargo fmt --all -- --check                              # PASS (repository-wide)
cargo test sandbox::generic_tests::developer_tool_environment_forces_the_sandbox_marker --locked  # PASS
cargo test upgrade_transaction --locked                 # PASS: 16/16
cargo test healthz --locked                             # PASS: 1/1
cargo clippy --all-targets --locked -- -D warnings      # PASS
cargo check --no-default-features --all-targets --locked  # PASS (existing dead_code warnings only)
git diff --check                                        # PASS
```

The dev-offline broad suite still cannot exercise host-IPC/loopback integration tests
under macOS Seatbelt (the `/tmp/temote-mcp-<uid>` socket and loopback `EPERM` failures
described in Pass 4). That is a sandbox capability boundary, not a claim that the broad
suite is green.

### Git status

No commit or push was performed. All existing worktree changes were preserved.

## Pass 6 — gateway doctor session_availability stage (2026-09-12)

### Issue addressed

`issues/open/20260911-gateway-doctor-readiness.md`. This pass completes the last
recorded repository-local residue: the `session_availability` stage previously stayed
`not_checked` in every case.

### Files changed

- `src/doctor.rs`
- `docs/gateway.md`
- `docs/gateway.ja.md`
- `issues/open/20260911-gateway-doctor-readiness.md`
- `docs/opencode-implementation-report.md` (this report)

### What changed

- Added `summarize_session_availability` and `classify_session_availability` pure helpers
  plus `check_gateway_session_availability`.
- `check_federation_readiness` now runs the new stage after the remote probe. It reuses the
  existing read-only supervisor control protocol (`request_session_views` /
  `ControlRequest::List`) and reports bounded `listed_sessions`/`active_sessions` counts with
  the non-secret `host_id`.
- Removed the three `session_availability=not_checked` emissions from `check_gateway_remote`;
  the stage is now always determined locally when the gateway host identity is valid.
- A supervisor that cannot enumerate its inventory maps to `unavailable`, never `ready`.
  A confirmed inventory with no live (`active`/`starting`) session maps to `failed`, never
  `ready`: the gateway endpoint can be alive while the target host has no available session,
  and that `session_unavailable` state must not be reported as healthy.
- No MCP tool is dispatched and no session, lease, approval, or filesystem state is mutated.
- Added deterministic unit tests: listed/active counting (`active`/`starting` are live), the
  empty-inventory not-ready case, the non-empty inventory with no live session case, the
  live-session `ready` case, and path/credential non-disclosure.
- Updated the English/Japanese gateway operator guides and the issue notes.

### What was intentionally not changed

- No Cloudflare call, route/Access mutation, or credential use.
- The gateway Worker `/v1/hosts/status` payload is unchanged; its own
  `session_availability=not_checked` field remains, and the Rust doctor does not depend on it.
- No remote upgrade-coordinator or OpenCode persistent-lifecycle work.

### Checks

This environment exposes no shell/command-execution tool, so the Rust and gateway suites
could not be executed here. The change is type-checked by inspection only and must be
verified before merge:

```text
cargo test doctor
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
(cd gateway && npm test)
git diff --check
```

### Remaining blockers

- Live Cloudflare route, Access, host-agent lease, and multi-host evidence remain
  credential/deployment dependent.
- `session_availability` is not yet exposed through the remote `/v1/hosts/status` payload;
  doctor derives it locally from the supervisor the gateway serves.

### Git status

No commit or push was performed. All existing worktree changes were preserved.

## Pass 7 — session_availability zero-live-session correction (2026-09-12)

### Issue addressed

`issues/open/20260911-gateway-doctor-readiness.md`. This pass repairs a defect in Pass 6:
`classify_session_availability` returned `ready` whenever it could enumerate the inventory,
including an empty inventory (`active_sessions=0`). That contradicted the issue's requirement
to distinguish "gateway endpoint is alive but the target host has no available session".

### Files changed

- `src/doctor.rs`
- `docs/gateway.md`
- `docs/gateway.ja.md`
- `issues/open/20260911-gateway-doctor-readiness.md`
- `docs/opencode-implementation-report.md` (this report)

### What changed

- `classify_session_availability` now returns `failed` with a short non-secret remediation when
  `active_sessions == 0`. This covers both the empty inventory and the non-empty inventory with
  no live (`active`/`starting`) session. `active_sessions > 0` remains `ready`.
- Enumeration failure remains `unavailable`; it is not conflated with the determinate
  zero-live-session failure or with `ready`.
- Deterministic tests updated/added: the empty-inventory case now asserts `failed` (was `ready`),
  a new non-empty no-live-session case asserts `failed`, and the non-disclosure test now covers
  the ready, no-live, and empty details plus the remediation hint.
- Operator docs and the issue implementation notes now state the corrected `failed` semantics and
  no longer claim an empty inventory is `ready`.

### Checks

Post-agent verification was run through the Temote session:

- `cargo fmt --all -- --check`: PASS after applying `cargo fmt --all` to the newly added tests.
- `cargo test doctor`: PASS, 26 passed / 0 failed in the filtered doctor suite, including all new
  `session_availability` tests.
- `cargo check --no-default-features --all-targets`: PASS (exit 0; existing dead-code warnings only).
- `cargo clippy --all-targets -- -D warnings`: PASS.
- `(cd gateway && npm test)`: PASS, 67 passed / 0 failed.
- `git diff --check`: PASS.
- `cargo test --all-targets --all-features --locked`: environment-limited in the normal `agent`
  sandbox: 591 passed / 93 failed / 1 ignored. The failures are unrelated host-state tests that
  require access under `~/Library/Application Support/temote-mcp/...`, which the repository-scoped
  sandbox correctly rejects with `Operation not permitted`. The new doctor tests passed in this run.

### Git status

No commit or push was performed. All existing worktree changes and the untracked `.worktrees/`
directory were preserved.

## Pass 8 — MCP handshake process identity (2026-09-13)

### Issue addressed

`issues/open/20260908-07-client-safe-upgrade-reconnect.md` (endpoint generation-identity
remainder). This pass stays repository-local and read-only: it exposes the existing
non-secret process identity through the MCP handshake so a reconnecting client can
verify host/version/boot generation, without adding remote upgrade tools, transport
barriers, or credential-dependent calls.

### Files changed

- `src/mcp.rs`
- `issues/open/20260908-07-client-safe-upgrade-reconnect.md`
- `docs/opencode-implementation-report.md` (this report)

### What changed

- Added `process_identity()` and `process_identity_meta()` in `src/mcp.rs`, returning
  `host_id` (validated stable host identity with the existing OS-hostname fallback),
  the running package `version`, and the existing per-process `boot_generation`.
- `initialize`, `ping`, and `server/discover` now include
  `_meta["io.temote/processIdentity"]`.
- `modernize_result` now merges the modern `serverInfo` entry into an existing `_meta`
  object instead of replacing it, so modern `initialize`/`ping` keep the identity field
  while retaining the existing serverInfo metadata.
- No MCP tool, schema, gateway contract, session, filesystem, or approval behavior
  changed; the added values are non-secret and bounded.

### Checks

Post-agent verification was run through the Temote session:

```text
cargo test process_identity                        # PASS: 4 passed / 0 failed
cargo test mcp::                                   # 82 passed / 6 failed (see note)
cargo fmt --all -- --check                         # PASS
cargo clippy --all-targets -- -D warnings          # PASS
cargo check --no-default-features --all-targets    # PASS (5 existing dead_code warnings)
(cd gateway && npm test)                           # PASS: 67 passed / 0 failed
git diff --check                                   # PASS
```

`cargo test process_identity` passed 4/4, including the three new MCP identity tests plus
the existing healthz identity test. All new identity tests also pass in `cargo test mcp::`
(82 passed / 6 failed). The six failures are environment/Temote sandbox-host-state failures
unrelated to this change:

- `agent_dev_tool_run_executes_without_a_local_console`
- `agent_git_fetch_skips_the_local_console`
- `ask_git_fetch_still_fails_closed_without_a_console`
- `file_tools_reject_special_file_targets_without_blocking`
- `local_agent_run_requires_approval_and_denial_starts_no_job`
- `session_list_surfaces_ambiguous_probe_as_unknown`

Their errors are `Operation not permitted` / lifecycle-state or nested-sandbox constraints.

### Remaining blockers

- The one-shot upgrade coordinator, response-flush commit barrier, remote
  `upgrade_preflight`/`upgrade_apply`/`upgrade_status` tools, and macOS/Linux
  deliberate-disconnect E2E remain unimplemented.
- Live Cloudflare route/Access/lease evidence remains credential/deployment dependent.

### Git status

No commit or push was performed. All existing worktree changes and the untracked
`.worktrees/` directory were preserved.
