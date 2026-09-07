# Multi-host federation: one Temote MCP endpoint for macOS, Linux, and Windows/WSL2 hosts

## Summary

Extend the existing multi-host Cloudflare gateway from session-oriented routing into host-aware federation so a single configured Temote MCP endpoint can safely control multiple local machines.

Target hosts:

- macOS (native Temote)
- Linux (native Temote)
- Windows 11 via WSL2 initially
- native Windows as a later phase

The intended user experience is that ChatGPT, Codex, or another MCP client configures Temote once, then selects the target host and session without creating a separate MCP server entry per machine.

## Background

Direct `temote-mcp up` intentionally has a single-host contract per public endpoint. Reusing the same Cloudflare Tunnel token/hostname across multiple Temote hosts is unsafe because Cloudflare replica routing does not understand Temote session ownership while session state remains host-local.

The repository already contains a multi-host Cloudflare gateway based on Worker + Durable Objects and `temote-mcp gateway-agent`. It can expose one MCP endpoint and route calls by `session_id` to macOS, Linux, and WSL2 hosts over outbound HTTPS long polling.

However, the current model requires a local Temote session to be started first and a gateway agent to be attached per session. For users with several machines, the missing abstraction is the host itself.

## Goal

Make Temote operate as a federated set of local hosts behind one MCP endpoint.

Example topology:

```text
ChatGPT / Codex / MCP client
            |
            | one configured MCP endpoint
            v
  Cloudflare Gateway
  Worker + Durable Objects
      /       |       \
     v        v        v
 mac-main  win-main  linux-main
   |          |          |
 sessions   sessions   sessions
```

Each machine should run one supervisor and one host-level gateway agent. The MCP client should be able to discover hosts, inspect their capabilities, and start or address sessions on a selected host.

## Desired UX

Host configuration examples:

```text
mac-main
  platform = macos
  roots:
    src=/Volumes/devstorage/Developer
    work=~/work

win-main
  platform = wsl2
  roots:
    src=/mnt/d/Developer

linux-main
  platform = linux
  roots:
    src=/home/me/src
```

Host startup should look conceptually like:

```sh
temote-mcp supervisor
temote-mcp gateway-agent --host-id mac-main
```

The remote MCP API should allow flows such as:

```text
host_list()

session_start(
  host_id="linux-main",
  path="src/project-a",
  session_id="project-a"
)

session_info(
  host_id="linux-main",
  session_id="project-a"
)
```

Human-facing usage should make it natural to express intent such as:

```text
@temote mac-main の foo を確認
@temote linux-main の bar をビルド
@temote win-main の baz をテスト
```

## Proposed architecture

### 1. Host registry

Add a host-level registry owned by the gateway.

Each active host lease should include at least:

- `host_id`
- detected/effective `platform`
- agent `instance_id`
- generation/fencing value
- supported capabilities
- advertised named roots, without exposing local absolute paths unnecessarily to unauthorized callers
- lease expiry / last seen
- supervisor/runtime protocol compatibility metadata

`host_id` is a stable non-secret routing identity. It must not be treated as authentication material.

### 2. Host-level gateway agent

Change or extend `temote-mcp gateway-agent` so one agent represents a whole Temote supervisor rather than one already-started session.

Responsibilities:

- maintain a host lease with the gateway
- expose host metadata and capabilities
- receive lifecycle requests destined for the local supervisor
- forward session tool calls to the correct local runtime
- preserve generation fencing across reconnects/restarts
- fail closed when the supervisor or target session is unavailable

The agent should use the existing owner-only local supervisor control path rather than bypassing local lifecycle ownership.

### 3. Host-aware lifecycle API

Add host discovery and make lifecycle operations explicitly host-aware.

Proposed MCP tools / fields:

```text
host_list()
host_info(host_id)

session_list(host_id?)
session_start(host_id, path, session_id)
session_info(host_id, session_id)
session_stop(host_id, session_id)
session_restart(host_id, session_id)
```

Exact API shape may differ if compatibility is better preserved with optional routing metadata, but host ownership must be explicit and deterministic.

### 4. Session identity and collision handling

Two hosts must be able to use the same human-friendly session ID without ambiguity.

Conceptual internal key:

```text
<host_id>/<session_id>
```

Examples:

```text
mac-main/srmj
linux-main/srmj
win-main/srmj
```

Prefer keeping `host_id` and `session_id` as separate API fields rather than forcing callers to concatenate them.

For backwards compatibility, existing unqualified session routing may remain valid only where it is unambiguous or under an explicitly defined default-host policy. Ambiguity must fail closed rather than silently choosing a host.

### 5. Gateway routing

Reuse the existing Worker/Durable Objects model where possible.

Possible structure:

- `GatewayHost`: host lease, generation, capabilities, lifecycle queue
- `GatewaySession`: per-host-session request queue / pending response state
- `GatewayRegistry`: active host/session discovery

The implementation should avoid introducing a central stateful process outside the existing gateway architecture unless needed.

## Security and safety requirements

Preserve the current Temote trust boundaries.

- Remote `session_start` remains limited to configured named roots and relative paths.
- Remote lifecycle calls must not create `--yolo` sessions.
- Sandbox enforcement remains on the execution host.
- Approval remains on the execution host.
- Gateway performs routing/coordination, not local policy enforcement bypasses.
- Host authentication remains separate from `host_id`.
- Existing Access service-token and host-token separation is preserved.
- Host reconnect increments generation and stale agent instances are fenced out.
- Non-idempotent operations must never be automatically replayed after timeout, disconnect, Worker replacement, or ambiguous delivery state.
- Lease expiry must fail pending calls safely.
- A host must not be able to impersonate another configured host merely by selecting its `host_id`; registration/lease ownership must be bound to authenticated agent state.
- Avoid leaking host-local filesystem layout beyond what is necessary for authorized named-root selection.

## Windows support plan

### Phase 1

Supported federation targets:

- Apple Silicon macOS native
- Linux native
- Windows 11 through WSL2

This matches the current Temote support model and allows multi-machine federation without first solving native Windows sandbox/transport semantics.

### Phase 2

Track native Windows as a separate implementation milestone, including at minimum:

- Windows filesystem/path semantics
- PowerShell / `cmd.exe` process execution behavior
- ACL and sandbox model
- owner-only local control transport (for example named pipes or another appropriate Windows primitive)
- service/supervisor lifecycle
- tests for path escape, symlink/reparse-point behavior, and privilege boundaries

Native Windows support must not weaken the Unix/macOS/Linux security model.

## Compatibility / migration

The existing session-oriented gateway path should not be broken abruptly.

Migration options to evaluate:

1. keep the current per-session `gateway-agent --session-id` mode temporarily and add host mode alongside it;
2. introduce host mode as the default and provide a compatibility shim for old gateway agents;
3. version the gateway agent protocol so mixed-version deployment fails clearly rather than misrouting calls.

The gateway MCP protocol compatibility contract and generated schema snapshots must be updated together with Rust and Node tests.

## Observability

Add diagnostics that make multi-host failures understandable without exposing secrets.

At minimum:

- `host_id`
- platform
- online/offline state
- generation
- agent instance identity (non-secret opaque ID)
- lease age / expiry reason
- protocol compatibility state
- session ownership by host
- routing errors that distinguish host offline, session missing, stale generation, and ambiguous session identity

`temote-mcp doctor` should report effective host identity and federation readiness when gateway host mode is configured.

## Implementation phases

### Phase A: protocol and registry

- define host identity and host lease schema
- add host registry Durable Object/state
- add generation fencing tests
- add `host_list` / `host_info` contracts
- preserve old session routing behavior

### Phase B: host-level agent

- connect one gateway agent to the local supervisor
- register host metadata and capabilities
- route lifecycle requests through the supervisor
- route session tool calls through host ownership
- add reconnect / lease-expiry / stale-generation tests

### Phase C: host-aware MCP lifecycle

- add `host_id` routing to session lifecycle APIs
- define collision/ambiguity behavior
- update `session_list` to support host filtering and host attribution
- update docs and generated contract snapshots

### Phase D: migration and operational hardening

- compatibility path for old per-session gateway agents
- rolling upgrade behavior
- diagnostics / `doctor`
- Cloudflare Worker deployment migration tests
- multi-host failure injection tests

### Phase E: native Windows follow-up

- separate issue/workstream for native Windows transport, sandbox, and lifecycle support

## Acceptance criteria

- One MCP endpoint can concurrently expose at least three hosts: macOS, Linux, and Windows/WSL2.
- Each host runs one supervisor and one host-level gateway agent regardless of the number of sessions on that host.
- `host_list` reports only currently valid leased hosts and does not show stale hosts as online.
- A remote client can start a managed session on a selected host by named-root-relative path.
- A remote client can list and address sessions with explicit host attribution.
- The same `session_id` can exist on two different hosts without collision.
- An unqualified ambiguous session lookup fails closed.
- Remote lifecycle operations cannot create yolo sessions.
- Local sandbox and approval semantics remain unchanged.
- Stale agent generations cannot receive requests or submit responses.
- Disconnect/timeout paths never auto-replay non-idempotent operations.
- Host authentication is not derived from or weakened by the human-readable `host_id`.
- macOS and Linux native hosts pass integration tests.
- Windows 11 through WSL2 passes integration tests.
- Existing direct single-host ingress remains supported.
- Existing gateway deployments have a documented migration path.
- Japanese and English documentation describe the multi-host setup and trust boundaries.

## Non-goals

- Replacing local Temote supervisors with a centralized remote executor.
- Sharing one local sandbox or approval state across hosts.
- Making Cloudflare routing authoritative for local filesystem policy.
- Native Windows support in the first federation release.
- Automatic retry/replay of ambiguous mutating tool calls.

## Expected value

This makes Temote substantially easier to use for users who have multiple development machines. Instead of configuring one MCP server per device, the user configures Temote once and treats local machines as federated execution targets while preserving per-host ownership, sandboxing, approvals, and failure isolation.
