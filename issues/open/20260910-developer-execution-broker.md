# Developer Execution Broker — Cargo / Vite+ / local AI agent delegation

## Status

Implemented. `local_agent_run` for Codex/OpenCode landed in PR #13, and the remaining `dev_tool_run` work (Slices B-E) is complete:

- `dev_tool_run({session_id, tool, operation, args?, cwd?})` is registered with an exact-key, no-executable schema; Cargo and Vite+ operations are classified as `dev-offline` (network disabled) or `dependency-network` (explicit network profile), while `vp run|exec|dlx`, `vp upgrade|implode`, unknown operations, and caller-selected executables/raw argv stay rejected.
- Offline operations run in a dedicated developer sandbox (workspace write plus narrowly scoped tool cache/state roots, top-level Git metadata protected, network disabled); dependency/network operations reuse the same containment with the explicit network capability.
- Approval is policy-driven: `ask` prompts, the default sandboxed `agent` mode skips only the Temote-local prompt after structural validation, and `yolo` keeps its existing behavior.
- Deterministic tests cover the classifier tables, argv construction, unsafe-class rejection, cwd containment, network-class selection, a sandboxed fake-tool run with outside-write denial, Agent approval-free execution, and the gateway contract parity snapshot.

See `docs/evaluations/agent-mode-release-readiness-20260912.md` for the verification record.

Do not reimplement or replace the existing `local_agent_run` broker while completing this issue. Changes to the default approval behavior for that broker belong to `20260911-default-agent-permission-mode.md`.

## Current state on `main`

The merged local-agent broker already provides the important shape this issue proposed for agent delegation:

- structured `local_agent_run` rather than caller-controlled raw argv;
- Codex/OpenCode adapter selection;
- canonical session-root cwd validation;
- bounded output/job ownership/cancellation;
- minimized child environment and isolated agent state;
- dedicated local-agent sandbox profiles rather than whole-session yolo;
- public HTTP/gateway contract coverage and operator documentation.

Still missing from this issue:

- `dev_tool_run` MCP surface;
- Cargo operation classification and execution;
- Vite+ operation classification and execution;
- dependency/network and arbitrary-code-sensitive policy for those tools;
- deterministic tests proving that selecting `cargo` or `vp` cannot become generic host execution.

## Background

Temote normal sessions intentionally keep filesystem access path-scoped and disable network access for ordinary `execute` / `start_command` calls. `--yolo` intentionally removes those Temote boundaries.

That binary split is awkward for day-to-day development workloads:

- Cargo often needs writable tool/cache state under Cargo/Rust homes and may need registry/network access.
- Vite+ (`vp`) is used frequently for install/add/update as well as check/lint/fmt/test/build/run/exec/dlx workflows.
- Codex and OpenCode are useful implementation workers, but they need controlled workspace write access, their own state/cache, and sometimes network access.
- Making the whole Temote session yolo is too broad and can conflict with MCP-client safety/authorization policy.

The desired model is therefore not “more yolo”. It is narrowly scoped host-capability brokering, following the existing `git_*`, kintone, 1Password, and now `local_agent_run` integration patterns.

## Goal

Complete the **Developer Execution Broker** so that a normal `yolo=false` Temote session can run approved Cargo and Vite+ development operations without exposing generic unrestricted host execution on the public MCP surface.

Remaining public capability:

1. `dev_tool_run` for Cargo and Vite+.

Already implemented and preserved as a sibling capability:

2. `local_agent_run` for Codex and OpenCode.

## Non-goals

- Do not expose `without_sandbox` through the public HTTP MCP endpoint.
- Do not allow remote clients to create or promote yolo sessions.
- Do not make `$HOME` globally writable.
- Do not treat executable-name allow-listing as a sufficient security boundary.
- Do not silently approve network, package installation, self-update, arbitrary project scripts, or local-agent execution.
- Do not redesign `local_agent_run` as part of the Cargo/Vite+ work unless a concrete shared-policy defect requires a narrow change.

## Proposed model

```text
remote MCP client
      |
      v
Temote normal session (yolo=false)
      |
      +-- ordinary execute/start_command
      |     workspace-scoped sandbox
      |     network denied
      |
      +-- Developer Execution Broker
            +-- dev_tool_run            # remaining work
            |     +-- cargo
            |     `-- vp
            |
            `-- local_agent_run         # already implemented
                  +-- codex
                  `-- opencode
```

The broker is a structured capability boundary. It must validate the requested operation, cwd, filesystem scope, environment, network policy, and approval requirement before launching host-side work.

## `dev_tool_run`

Proposed public contract:

```text
dev_tool_run
  session_id: string
  tool: cargo | vp
  operation: string
  args: string[]
  cwd?: string
```

Do not accept an arbitrary executable path.

### Cargo operation classes

#### Development / normally network-denied

Examples:

- `fmt`
- `check`
- `clippy`
- `test`
- `build`

These are still code-execution-capable because Cargo may execute build scripts, proc macros, test binaries, and project code. The broker must therefore preserve workspace/path containment even when network is denied.

#### Dependency / network-capable

Examples:

- `fetch`
- `install`
- `update`
- registry-backed operations that require index/download access

These require explicit policy classification and, while `ask` mode remains in use, the appropriate local approval. They receive only the minimum writable Cargo/Rust cache/config paths needed by the operation. The later `agent` permission-mode issue may remove the Temote approval prompt, but it must not change this capability classification.

### Vite+ operation classes

#### Development / normally network-denied

Examples:

- `check`
- `lint`
- `fmt` / `format`
- `test`
- `build`
- `pack`

#### Dependency / network-capable

Examples:

- `install`
- `add`
- `update`
- `outdated`
- `info`
- `rebuild` when dependency acquisition is required

#### Arbitrary-code-sensitive

Examples:

- `run`
- `exec`
- `dlx`
- package lifecycle scripts triggered by install/rebuild

These must never be treated as safe merely because the top-level executable is `vp`. They require an explicit policy class. `dlx` additionally combines network download with arbitrary code execution.

#### Self-mutation

Examples:

- `upgrade`
- `implode`

Keep these outside the initial implementation or require a separate explicit capability/approval contract.

## Existing `local_agent_run` boundary

The merged broker should be treated as an implementation dependency, not unfinished work in this issue.

Preserve these behaviors while introducing shared developer-policy helpers:

- resolve `cwd` through the selected Temote session and permitted roots;
- caller supplies a bounded task, not arbitrary agent CLI argv;
- selected workspace/access mode remains explicit;
- child environment is minimized and Temote startup secrets are not inherited implicitly;
- bounded stdout/stderr and Temote-owned background jobs remain in force;
- cancellation/session stop terminates the child process tree;
- agent-specific approval/sandbox behavior remains separate from Temote authorization.

If policy code is shared between `dev_tool_run` and `local_agent_run`, add regression tests proving the existing agent contract did not broaden.

## Sandbox profiles

The implementation should model capabilities explicitly rather than as `yolo=true/false` internally.

### `strict`

Existing ordinary command behavior:

- permitted workspace write
- protected metadata rules unchanged
- network denied

### `dev-offline`

For development commands that need tool state but not network:

- permitted workspace write
- narrowly scoped tool cache/state write
- network denied
- project code execution allowed only inside the constrained process/filesystem boundary

### `dependency-network`

For dependency resolution and registry operations:

- permitted workspace write
- narrowly scoped Cargo / Vite+ / package-manager cache and state write
- outbound network enabled
- authorization determined by the session permission-mode policy, without widening the capability itself

### `agent-workspace`

Already used by the local-agent broker. Preserve its current reviewed implementation rather than replacing it to make the Cargo/Vite+ work fit.

Do not implement any of these profiles by making the whole session yolo.

## Security invariants

The change must preserve all existing Temote safety invariants, including:

- Public HTTP does not expose `without_sandbox`.
- Managed sessions remain `yolo=false` and cannot self-promote remotely.
- Filesystem paths remain canonicalized and contained within configured session roots plus narrowly declared broker-owned state/cache roots.
- Symlink escapes remain rejected.
- Git metadata remains protected except through existing dedicated Git operations or an explicitly reviewed broker requirement.
- Secrets are not copied into logs, approval summaries, session metadata, or ordinary command output.
- Network enablement is a capability decision, not a side effect of selecting a command name.
- Child commands cannot request arbitrary extra host paths or silently broaden environment inheritance.
- Reusing local-agent sandbox/policy code must not weaken the already-merged `local_agent_run` contract.

## Executable implementation slices

Each slice should be independently reviewable and testable by a local implementation agent.

### Slice A — Cargo/Vite+ policy classifier only

- Add backend-neutral developer-tool request/policy types.
- Classify Cargo and Vite+ operations into `dev-offline`, `dependency-network`, `arbitrary-code-sensitive`, or rejected/self-mutation.
- Reject unknown tool names, invalid operations, NUL/oversized args, and caller-controlled executable paths.
- Add table-driven/property tests for every listed operation class and unknown/dangerous cases.
- Do not expose a new MCP tool or launch a child process in this slice.

### Slice B — Cargo offline execution

- Add `dev_tool_run` plumbing for `tool=cargo` and offline development operations only.
- Canonicalize cwd and reuse the narrow developer sandbox/state model.
- Keep network disabled.
- Cover `fmt`, `check`, `clippy`, `test`, and `build`, including build-script/proc-macro containment tests.
- Update gateway contract only for the exact new structured surface.

### Slice C — Vite+ offline execution

- Add `tool=vp` for offline-classified operations only.
- Pin command construction inside Temote; no raw argv/executable passthrough.
- Prove `run`, `exec`, `dlx`, dependency operations, and self-mutation cannot enter the offline path.
- Add fake-tool deterministic tests before any live acceptance.

### Slice D — dependency/network operations

- Add the explicit network-enabled profile only after A-C pass.
- Scope writable tool caches/state narrowly.
- Map authorization through the current permission-mode policy (`ask`/future `agent`) without turning classification into approval bypass.
- Cover Cargo fetch/update/install and Vite+ install/add/update behavior plus secret/environment isolation.

### Slice E — docs and live acceptance

- Update the narrowest English/Japanese operator docs and `skills/temote-mcp/SKILL.md` only for the final surface.
- Add Linux/macOS deterministic checks where platform behavior differs.
- Record credential/network-dependent evidence in the live acceptance matrix rather than keeping implementation slices open.

## Acceptance criteria

- [ ] A normal `yolo=false` session can run representative Cargo check/test/clippy/build workflows without requiring the whole session to become yolo.
- [ ] A normal session can run representative `vp check`, `vp test`, and `vp build` workflows.
- [ ] Dependency/network operations are explicitly classified and authorized according to permission mode.
- [ ] `vp run`, `vp exec`, and `vp dlx` cannot bypass arbitrary-code/network policy by virtue of the `vp` executable name.
- [ ] Cargo build scripts/proc macros/tests remain filesystem-contained according to the selected developer profile.
- [x] Codex and OpenCode can be invoked through a structured `local_agent_run` contract without caller-controlled arbitrary argv (PR #13).
- [x] Child agent environment inheritance is minimized and does not expose Temote-held credentials by default (PR #13).
- [x] Public HTTP still omits `without_sandbox` and cannot create/promote yolo sessions after the local-agent broker change.
- [ ] Existing `local_agent_run`, Git, 1Password, kintone, session lifecycle, approval, and sandbox regression tests continue to pass after `dev_tool_run` lands.
- [ ] `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, gateway tests, and `git diff --check` pass before merge.

## Implementation status (2026-09-12)

- Slice B/C: `dev_tool_run` executes `cargo fmt|check|clippy|test|build` and `vp check|lint|fmt|format|test|build|pack` offline with workspace + scoped tool-cache writes and network disabled.
- Slice D: `cargo fetch|install|update` and `vp install|add|update|outdated|info|rebuild` use the explicit dependency-network scope; authorization flows through the shared permission-mode policy.
- Slice E: operator docs (`docs/usage.md`/`.ja.md`) and the Agent Skill document the final surface. Live Cargo acceptance has a local ignored test (`live_cargo_check_in_the_developer_sandbox`); Vite+ live acceptance depends on an installed `vp` and remains an operator check.

## Recommended next implementation slice

None required for the broker contract. Future work would be live Vite+ acceptance on a host with `vp` installed and any further capability classes the product explicitly approves.
