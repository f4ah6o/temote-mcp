# Session lifecycle cleanup — safely forget stale / terminal session metadata

## Status

First implementation slice landed on main: `temote-mcp session forget <id>` is parsed, routed through `ControlRequest::Forget`, serialized by the supervisor with lifecycle transitions, and removes terminal session metadata, lifecycle state, and a confirmed-stale socket entry with symlink/special-file rejection. Live sessions are refused by an unconditional runtime socket probe. Retention policy is unchanged.

## Background

Temote persists session metadata and lifecycle state so `session list` / `session info` can report active, stopped, and crashed sessions across supervisor restarts.

That durability is useful, but there is currently no supported CLI operation to explicitly remove one obsolete session record. The local CLI exposes lifecycle operations such as `start`, `list`, `info`, `stop`, `restart`, restart-policy management, and permission management, but not a `delete` / `forget` operation.

When a session is no longer useful — for example, its worktree was already removed or the previous owning supervisor is gone — operators can end up with stale metadata that cannot be cleaned up through the supported lifecycle surface. The fallback is manual filesystem deletion under Temote's private state directory and socket directory.

Manual deletion is undesirable because callers must know Temote's platform-specific private paths and must independently prove that the target session is not live.

## Problem

Provide an explicit, safe lifecycle operation for removing durable state for an obsolete session without weakening the ownership and liveness protections of the supervisor.

The operation must distinguish:

- stopping a live runtime,
- retaining terminal session history,
- and intentionally forgetting one terminal / orphaned session record.

It must not turn a metadata-cleanup command into an alternate way to kill or detach an active runtime.

## Proposed CLI

Prefer the semantic name `forget`:

```text
temote-mcp session forget <session-id>
```

`delete` can be considered as an alias only if needed for discoverability. The implementation should have one canonical internal operation.

### Expected behavior

`session forget <id>` should:

1. Validate the session ID using the existing session-ID validation rules.
2. Probe the session runtime socket using the existing liveness contract.
3. Refuse if the session is live or in a supervisor-owned in-flight lifecycle transition.
4. Remove Temote-owned durable artifacts for that session, including lifecycle metadata, session metadata, and a confirmed-stale socket entry when present.
5. Leave the workspace/worktree and all non-Temote files untouched.

A missing or already-removed working directory must not prevent cleanup. Forgetting is about Temote-owned lifecycle state, not about canonicalizing a workspace that no longer exists.

## Safety invariants

The implementation must preserve these boundaries:

- **Never forget a live session.** A successful runtime socket probe is an unconditional refusal.
- **Do not trust lifecycle metadata alone for liveness.** Metadata can be stale after crashes or supervisor replacement.
- **Do not bypass current-supervisor ownership rules for active sessions.** `forget` is terminal-state cleanup, not a replacement for `stop`.
- **Do not recursively delete the session cwd, permitted directories, worktrees, repositories, Codex state, or user project files.** Only artifacts explicitly owned by the Temote session lifecycle may be removed.
- **Reject symlink / special-file substitution for metadata targets.** Reuse the existing no-follow / regular-file safety model where applicable.
- **Keep public HTTP semantics conservative.** Initial support should be local CLI / local control only unless a separate review justifies exposing destructive lifecycle cleanup remotely.

## Orphaned-session case

The motivating class of failure is a session whose persisted identity remains relevant to a client, but whose runtime is not owned by the current supervisor and may reference a cwd that has already disappeared.

Example symptom:

```text
Error: session <id> is not managed by this supervisor process
```

Today the operator has no first-class command to say: “this runtime is gone; remove Temote's retained lifecycle state for this ID.”

`session forget` should cover this case without requiring the old cwd to exist.

## State cleanup contract

The implementation should centralize the set of Temote-owned per-session artifacts instead of duplicating paths in the CLI layer.

At minimum, review and handle:

```text
<state>/temote-mcp/sessions/<id>.json
<state>/temote-mcp/sessions/<id>.state
<socket-dir>/<id>.sock   # only after confirming it is not live
```

If additional durable per-session artifacts exist, the implementation must either include them in the forget contract or document why they intentionally survive forgetting.

Cleanup should be bounded and deterministic. It must not scan arbitrary directories or infer paths from untrusted metadata.

## Supervisor interaction

The preferred architecture is a dedicated local-control request handled by the lifecycle supervisor, for example:

```text
ControlRequest::Forget { session_id }
```

The supervisor should serialize `forget` with existing lifecycle transitions so that `start`, `stop`, `restart`, permission mutation, automatic restart, upgrade fencing, and forget cannot race each other.

If the session ID is currently present in the supervisor's in-memory runtime / restart structures, forgetting should fail unless the runtime has already reached a terminal state and the supervisor has deterministically removed ownership state first.

## Retention interaction

Existing automatic retention of terminal metadata and explicit `session forget` solve different problems:

- retention bounds historical storage globally;
- forget is an operator-requested removal of one known obsolete session.

Adding `forget` must not silently change the existing retention policy.

## Acceptance criteria

### CLI contract

- `temote-mcp session forget <id>` is parsed, documented, and routed through the local supervisor control protocol.
- Help / usage text clearly distinguishes `stop` from `forget`.

### Live-session refusal

- Active session with a responsive runtime socket: **refused**, no lifecycle artifacts removed.
- Starting / stopping session owned by the current supervisor: **refused**, no partial cleanup.
- A race where a runtime becomes live during cleanup must fail closed.

### Terminal cleanup

- Gracefully stopped session: metadata can be forgotten.
- Crashed session: metadata can be forgotten.
- Session whose cwd/worktree no longer exists: metadata can still be forgotten.
- Confirmed stale socket path is removed.
- After success, `session list` / `session info` no longer surface the forgotten session.

### Filesystem safety

- Symlink metadata target is rejected rather than followed.
- Non-regular metadata target is rejected.
- Cleanup never removes the cwd or project files.
- Cleanup is limited to deterministic Temote-owned paths derived from validated session ID and platform state/socket roots.

### Concurrency / restart safety

- Forget is serialized against start/stop/restart/automatic restart/permission transitions.
- Supervisor upgrade fencing prevents forget during an incompatible in-flight handoff.
- Forgetting a terminal session does not affect any other session.

### Tests

Add focused unit/integration coverage for:

- stopped session forget,
- crashed/orphaned session forget,
- missing cwd,
- live runtime refusal,
- stale socket cleanup,
- symlink/special-file rejection,
- concurrent lifecycle mutation refusal/serialization,
- and list/info behavior after forget.

## Non-goals

- Do not add recursive workspace/worktree deletion.
- Do not make `forget` implicitly stop live sessions.
- Do not add a broad `purge all` command in the first implementation.
- Do not expose arbitrary state-directory deletion primitives.
- Do not weaken session retention defaults as a substitute for explicit cleanup.

## Implementation notes

Likely touch points include:

```text
src/cli.rs
src/main.rs
src/session_control.rs
src/supervisor.rs
src/config.rs
```

The implementation should reuse existing session validation, socket probing, lifecycle transition locking, no-follow metadata handling, and platform-path resolution rather than introducing a second cleanup path.

## Done when

An operator can safely remove a known obsolete Temote session with one supported command, including when its old worktree no longer exists, while a live session remains impossible to forget accidentally.
