# Developer Execution Broker — Cargo / Vite+ / local AI agent delegation

## Status

Design issue. Runtime implementation has not started.

## Background

Temote normal sessions intentionally keep filesystem access path-scoped and disable network access for ordinary `execute` / `start_command` calls. `--yolo` intentionally removes those Temote boundaries.

That binary split is awkward for day-to-day development workloads:

- Cargo often needs writable tool/cache state under Cargo/Rust homes and may need registry/network access.
- Vite+ (`vp`) is used frequently for install/add/update as well as check/lint/fmt/test/build/run/exec/dlx workflows.
- Codex and OpenCode are useful implementation workers, but they need controlled workspace write access, their own state/cache, and sometimes network access.
- Making the whole Temote session yolo is too broad and can conflict with MCP-client safety/authorization policy.

The desired model is therefore not “more yolo”. It is narrowly scoped host-capability brokering, following the existing `git_*`, kintone, and 1Password integration pattern.

## Goal

Add a **Developer Execution Broker** that lets a normal `yolo=false` Temote session delegate approved development operations without exposing generic unrestricted host execution on the public MCP surface.

Initial capabilities:

1. `dev_tool_run` for Cargo and Vite+.
2. `local_agent_run` for Codex and OpenCode.

## Non-goals

- Do not expose `without_sandbox` through the public HTTP MCP endpoint.
- Do not allow remote clients to create or promote yolo sessions.
- Do not make `$HOME` globally writable.
- Do not treat executable-name allow-listing as a sufficient security boundary.
- Do not silently approve network, package installation, self-update, arbitrary project scripts, or local-agent execution.

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
            +-- dev_tool_run
            |     +-- cargo
            |     `-- vp
            |
            `-- local_agent_run
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

These require explicit local approval in a normal session and receive only the minimum writable Cargo/Rust cache/config paths needed by the operation.

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

These must never be treated as safe merely because the top-level executable is `vp`. They require an explicit policy class and local approval in normal sessions. `dlx` additionally combines network download with arbitrary code execution.

#### Self-mutation

Examples:

- `upgrade`
- `implode`

Keep these outside the initial implementation or require a separate explicit capability/approval contract.

## `local_agent_run`

Proposed public contract:

```text
local_agent_run
  session_id: string
  agent: codex | opencode
  task: string
  cwd?: string
  access: read_only | workspace_write
  profile?: string
```

The caller provides a task, not arbitrary agent CLI argv.

### Required behavior

- Resolve `cwd` through the selected Temote session and permitted roots.
- Normal sessions require local approval before host-side agent execution.
- Give the child agent only the selected workspace and explicit agent state/cache paths.
- Sanitize the inherited environment; do not forward Temote startup secrets or unrelated host credentials.
- Do not infer that the child agent may bypass Temote authorization or production-operation gates.
- Capture bounded stdout/stderr and return a normal foreground result or Temote-owned background job.
- Cancellation/session stop must terminate the child process tree.
- Agent-specific approval/sandbox behavior remains separate from Temote's own authorization decision.

## Sandbox profiles

The implementation should model capabilities explicitly rather than as `yolo=true/false` internally.

Suggested initial profiles:

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
- normal-session local approval required

### `agent-workspace`

For Codex/OpenCode:

- selected workspace access according to `read_only` / `workspace_write`
- narrowly scoped agent home/cache
- optional network according to an explicit policy
- normal-session local approval required

Do not implement these profiles by making the whole session yolo.

## Security invariants

The change must preserve all existing Temote safety invariants, including:

- Public HTTP does not expose `without_sandbox`.
- Managed sessions remain `yolo=false` and cannot self-promote remotely.
- Filesystem paths remain canonicalized and contained within configured session roots plus narrowly declared broker-owned state/cache roots.
- Symlink escapes remain rejected.
- Git metadata remains protected except through existing dedicated Git operations or an explicitly reviewed broker requirement.
- Secrets are not copied into logs, approval summaries, session metadata, or ordinary command output.
- Network enablement is a capability decision, not a side effect of selecting a command name.
- Child commands/agents cannot request arbitrary extra host paths or silently broaden environment inheritance.

## Implementation phases

### Phase 1 — contract and policy engine

- Add internal developer-execution request types and policy classification.
- Implement Cargo and Vite+ operation classification.
- Add unit/property tests for argument validation, path containment, network classification, and dangerous subcommands.
- No local-agent launch yet.

### Phase 2 — `dev_tool_run`

- Expose Cargo and Vite+ through the MCP tool surface.
- Add local approval for network/arbitrary-code-sensitive classes.
- Add bounded job/output handling using existing Temote runtime patterns.
- Add Linux live acceptance coverage.

### Phase 3 — `local_agent_run`

- Add Codex and OpenCode adapters behind one structured contract.
- Add environment minimization, workspace access modes, cancellation, and output handling.
- Prefer adapter-specific argv construction inside Temote; do not pass caller-controlled raw argv.

### Phase 4 — documentation and extension points

- Document the developer broker in `docs/usage*` or a narrow dedicated document.
- Update `skills/temote-mcp/SKILL.md` so agents prefer broker tools instead of yolo for supported development work.
- Keep the internal policy generic enough to add tools such as `pnpm`, `npm`, `uv`, `go`, or `zig` later without exposing a generic unrestricted executor.

## Acceptance criteria

- [ ] A normal `yolo=false` session can run representative Cargo check/test/clippy/build workflows without requiring the whole session to become yolo.
- [ ] A normal session can run representative `vp check`, `vp test`, and `vp build` workflows.
- [ ] Dependency/network operations are explicitly classified and local-approval-gated.
- [ ] `vp run`, `vp exec`, and `vp dlx` cannot bypass arbitrary-code/network policy by virtue of the `vp` executable name.
- [ ] Cargo build scripts/proc macros/tests remain filesystem-contained according to the selected developer profile.
- [ ] Codex and OpenCode can be invoked through a structured `local_agent_run` contract without caller-controlled arbitrary argv.
- [ ] Child agent environment inheritance is allow-listed/minimized and does not expose Temote-held credentials by default.
- [ ] Public HTTP still omits `without_sandbox` and cannot create/promote yolo sessions.
- [ ] Existing Git, 1Password, kintone, session lifecycle, approval, and sandbox regression tests continue to pass.
- [ ] `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, gateway tests, and `git diff --check` pass before merge.

## Recommended first implementation slice

Start with **Phase 1 only**: policy types + Cargo/Vite+ classifier + tests. This keeps the first PR reviewable and establishes the security model before any new host/network execution path is introduced.
