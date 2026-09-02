# Zero-downtime supervisor upgrade / handoff

Date: 2026-09-02

## Background

Temote MCP now separates two long-lived ownership boundaries:

- `temote-mcp supervisor` owns local session runtimes and their lifecycle state.
- `temote-mcp up --profile ...` owns the HTTP origin and ingress child, but does not own session runtimes.

This separation is correct, but it leaves an operational gap during binary upgrades.

A normal package update replaces the executable on disk, while the already-running supervisor process continues executing the old image. Re-running `temote-mcp up` can move ingress to the new binary, but it does not upgrade the session supervisor. Fully applying an upgrade therefore currently requires an operator to reason about and restart the supervisor separately, potentially interrupting many long-lived sessions.

Observed upgrade flow on 2026-09-02:

```text
installed binary: 2026.8.9 -> 2026.9.1 -> 2026.9.2
running supervisor: process started before those replacements
active sessions: many, all owned by that supervisor process
```

`temote-mcp up` alone is insufficient because the supervisor is an independent lifecycle owner. Requiring every operator to manually coordinate package installation, supervisor restart, session recovery, ingress restart, and Codex plugin refresh is error-prone.

## Problem statement

Temote should provide one explicit, fail-closed operational path that upgrades a running installation and transfers supervisor ownership safely.

The desired property is not merely "restart the process". It is:

> Replace the running supervisor with the installed/new supervisor implementation while preserving or automatically recovering the intended active-session set, with bounded interruption and rollback/failure semantics that do not strand lifecycle metadata.

If true live transfer of in-process session runtimes is not feasible, the command should still provide a coordinated drain/restart/restore operation rather than requiring manual reconstruction.

## Proposed CLI surface

Primary operator command:

```text
temote-mcp upgrade
```

Possible lower-level primitive:

```text
temote-mcp supervisor handoff
```

`upgrade` may orchestrate package/version checks only when Temote has a safe install mechanism to do so. Otherwise it may initially mean "apply the currently installed binary to running Temote services" and report that package installation remains external.

Avoid changing `temote-mcp up` so that it silently restarts the session supervisor. `up` should remain scoped to HTTP/ingress ownership; an explicit upgrade/handoff command makes the lifecycle impact visible.

## Required behavior

### 1. Preflight and version identity

Before changing a running process, identify and report:

- installed executable path and version;
- running supervisor PID and version/build identity;
- local control-protocol version;
- active/stopped/crashed session counts;
- ingress state and profile, if active;
- whether Codex plugin state points at the same executable/version, when detectable locally.

If the installed binary is not newer/different from the running supervisor, the operation should be an idempotent no-op unless explicitly forced for repair.

### 2. Fail-closed compatibility gate

The new binary must verify that it can understand the existing durable lifecycle state before taking ownership.

Do not terminate the old supervisor merely because a newer executable exists.

At minimum, gate on:

- lifecycle state schema compatibility;
- local control-protocol compatibility;
- named-root configuration availability;
- session metadata needed to recreate each active session;
- permission mode and restart policy preservation;
- integration environment/credential restart requirements.

Sessions whose required restart inputs cannot be reconstructed must be reported before destructive action.

### 3. Handoff model

Prefer a staged handoff:

```text
old supervisor
  -> stop accepting lifecycle mutations
  -> snapshot/revalidate intended active sessions
  -> new supervisor preflight
  -> transfer ownership or coordinated runtime restart
  -> probe every restored session
  -> switch lifecycle control endpoint
  -> release old supervisor
```

Two implementation levels are acceptable:

#### Level A: live runtime transfer

If architecture permits safe socket/FD/process ownership transfer, existing session runtimes survive without restart.

This is the ideal result but should not be required if it would substantially weaken the current in-process ownership model.

#### Level B: coordinated restart/restore

If runtimes cannot survive supervisor replacement because they are in-process tasks, Temote should:

1. freeze lifecycle mutations;
2. record the exact active-session restart plan;
3. gracefully stop the old runtimes;
4. start the new supervisor;
5. recreate only the sessions that were active before the handoff;
6. preserve session IDs, logical paths, permission modes, restart policy, and compatible metadata;
7. probe each recreated socket before declaring success.

This still provides a single safe operator action and avoids manual session reconstruction.

### 4. Credential-bearing session restart

Current session restart captures integration environment from the caller again. Some sessions may depend on secret-injection wrappers or service-account environment that is intentionally not persisted in lifecycle metadata.

Upgrade must not silently recreate such a session without the required credential source.

Possible safe approaches:

- persist only a non-secret restart provider/recipe identifier that can reacquire credentials;
- allow the supervisor service definition to define the approved restart wrapper;
- classify a session as `restart_requires_operator_context` and abort before stopping the old supervisor unless that context is available.

Never persist plaintext credentials merely to make upgrade automatic.

### 5. Ingress coordination

If direct ingress is active, the command should coordinate it after the supervisor transition:

- verify the new supervisor/control protocol first;
- restart/reload HTTP origin/ingress only when required by binary/protocol change;
- preserve the configured profile and host identity;
- confirm post-upgrade origin health.

The command must preserve the existing contract that direct ingress does not itself own session runtimes.

### 6. Codex plugin reconciliation

When the installed Temote binary owns the local Codex plugin installation, `upgrade` should either:

- transactionally run the equivalent of `temote-mcp codex plugin install`, or
- report a precise required follow-up when the plugin cannot be changed from that environment.

An already-running Codex/ChatGPT client may still require restart; Temote should report this explicitly rather than pretending the loaded plugin has changed in-process.

### 7. Rollback / failure semantics

Before terminating the old supervisor, all non-destructive validation should pass.

If the new supervisor cannot start or restore the intended active-session set:

- do not discard the captured restart plan;
- preserve lifecycle evidence and error details;
- where technically safe, restart/re-enable the old supervisor executable/process generation;
- otherwise fail with an explicit partial-state report listing restored and unrestored sessions.

Never report upgrade success solely because the new supervisor process started. Success requires lifecycle/control health plus the intended session set reaching the expected state.

## Suggested status command

A reusable diagnostic surface would make upgrades inspectable:

```text
temote-mcp status
```

Example information:

```text
installed:   2026.9.2 /home/user/.cargo/bin/temote-mcp
supervisor:  2026.9.1 pid=1234
protocol:    compatible
sessions:    34 active, 2 stopped
 ingress:    cloudflare active
plugin:      2026.9.2; client restart required
upgrade:     supervisor handoff required
```

This is not a prerequisite if equivalent information is exposed through `upgrade --dry-run`.

## Dry-run

Provide:

```text
temote-mcp upgrade --dry-run
```

It should produce the exact transition plan without stopping or restarting anything, including:

- old/new version identity;
- sessions that can be recreated automatically;
- sessions blocked by missing restart context;
- ingress actions;
- plugin actions;
- expected operator/client restart requirements.

## Non-goals

- silently upgrading crates/packages from the network without an explicit install contract;
- persisting plaintext integration credentials;
- treating `temote-mcp up` as the owner of session runtimes;
- cross-host migration or HA failover;
- changing remote authorization/yolo boundaries;
- guaranteeing zero TCP interruption if session runtimes must be recreated.

## Acceptance criteria

- [x] an explicit command can determine whether installed and running supervisor versions differ
- [ ] `--dry-run` produces a complete non-destructive handoff/restart plan
- [x] lifecycle mutations are fenced while supervisor ownership is changing
- [x] compatibility is verified before the old supervisor is stopped
- [x] intended active sessions are preserved or recreated automatically with the same IDs
- [x] logical path, cwd resolution, permission mode, and restart policy are preserved
- [x] sessions requiring unavailable secret/restart context block destructive transition
- [x] no plaintext credential is persisted to enable upgrade
- [x] every restored session is socket-probed before upgrade success
- [ ] ingress is restarted/reloaded only when needed and is health-checked afterward
- [x] Codex plugin version is reconciled or a precise client-restart action is reported
- [ ] failure produces a deterministic rollback/partial-state report
- [x] repeated invocation after successful upgrade is idempotent
- [x] existing `up/down`, session lifecycle, sandbox, approval, 1Password, kintone, and gateway security boundaries remain unchanged
- [ ] Linux and macOS upgrade-path tests cover compatible and incompatible generations
- [x] README / managed-session / usage documentation is updated in English and Japanese

## Required tests

1. installed version equals running version -> no-op
2. newer installed version + compatible lifecycle state -> successful handoff/restart plan
3. incompatible control/lifecycle protocol -> old supervisor remains untouched
4. active sessions are recreated with identical IDs and expected permission/restart metadata
5. one session lacking credential restart context blocks destructive handoff
6. new supervisor startup failure preserves recoverable state and reports rollback result
7. partial session restore is never reported as full success
8. ingress health is re-established after a successful supervisor transition
9. plugin reconciliation is transactional and reports client restart requirement
10. repeated upgrade after success is safe and idempotent

## Implementation note

The current architecture keeps session runtimes as in-process tasks owned by `SessionSupervisor`. Therefore Level B (coordinated restart/restore) is likely the smallest safe first implementation. A future Level A live-FD/runtime transfer can be considered separately if restart interruption becomes material.


## Implementation status — 2026-09-02

Implemented the first Level B handoff path:

- `temote-mcp upgrade --dry-run` compares the installed/current CLI binary with the running supervisor and requests a non-destructive plan.
- `temote-mcp upgrade` validates the target executable by invoking `supervisor --capabilities`; control protocol, lifecycle schema, and restore-plan schema must match the running supervisor before any runtime is stopped.
- The running supervisor fences lifecycle mutations and validates every active session's named-root/cwd mapping plus memory-only restart context. Missing/changed restart environment is reported by variable **name only** and aborts the transition.
- Active session-runtime bridge operations and approvals are counted. Upgrade quiesce fails closed while one is in flight and resumes any already-quiesced runtime on abort.
- The disk restore plan is owner-only and contains session identity/path/permission/restart state plus restart-context key names only. Credential values remain memory-only and are passed to the replacement process through the `exec` environment, not persisted.
- The old supervisor gracefully drains the planned sessions and `exec`s the target supervisor with `--restore-plan` in the same PID. The replacement recreates only the prior active set, preserves ID/logical path/cwd/permitted roots/yolo/restart policy, and probes every restored socket before deleting the plan.
- If `exec` fails before process replacement, the old supervisor attempts rollback from its still-memory-resident restart specifications and clears the lifecycle fence.
- After successful supervisor/session verification, `upgrade` transactionally runs the binary-owned Codex plugin installer. Failure produces the exact manual follow-up command; already-running Codex still requires restart.
- Same-version invocation is a no-op unless `--force` is supplied.

Current deliberate limitations keep this issue open:

- protocol/lifecycle-schema-changing generations fail closed; automatic `serve/up` ingress restart/profile recovery and post-transition ingress health checks are not implemented yet;
- if the replacement process has already `exec`'d and then fails during restore, the non-secret plan is retained but automatic execution of an old binary generation is not yet implemented;
- Linux isolated process E2E has been exercised; macOS process-boundary handoff evidence is still required.

### Evidence

- full `cargo test --all-targets --all-features`: 347 main-binary tests passed, 0 failed; existing process-boundary lifecycle test remains intentionally ignored; sandbox/package tests also passed;
- `cargo clippy --all-targets --all-features -- -D warnings`: passed;
- `cargo check --no-default-features --all-targets`: passed (existing feature-dependent dead-code warnings only);
- isolated Linux E2E using socket namespace `upge2e01`: supervisor PID `2233563` remained unchanged across forced same-version `exec` handoff; session `e2e-upgrade` was restored active with the same ID/cwd/permission metadata and then stopped cleanly;
- the E2E temporarily installed the checkout debug Codex plugin as part of the real upgrade path; the host plugin was immediately restored with the installed `temote-mcp codex plugin install`, returning it to version `2026.9.2`.
