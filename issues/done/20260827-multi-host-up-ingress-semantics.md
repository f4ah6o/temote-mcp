# Define safe multi-host semantics for `temote-mcp up`

Date: 2026-08-27

## Background

`temote-mcp up --profile cloudflare` starts an HTTP MCP origin and a managed `cloudflared` process on one host. Runtime state such as the lifecycle PID file, supervisor socket, session metadata, and session sockets is host-local.

If two hosts such as `ubuntu1` and `ubuntu2` are configured identically and both run `temote-mcp up`, the local state does not conflict because each machine has its own filesystem and loopback listener. However, if both hosts use the same Cloudflare Tunnel token/public endpoint, Cloudflare can treat both `cloudflared` processes as replicas of the same tunnel.

That creates an unsafe semantic mismatch: HTTP requests may reach either host, while Temote session state remains local to only one host. A request that creates or inspects a session is therefore not guaranteed to reach the host that owns that session.

Example failure:

```text
ChatGPT
  |
  v
same Cloudflare Tunnel / hostname
  |                    |
  v                    v
ubuntu1              ubuntu2
session: dagu        no dagu session
```

A `session_start("dagu")` routed to `ubuntu1` followed by `session_info("dagu")` routed to `ubuntu2` can fail or observe unrelated state. Using the same session ID on both hosts is also ambiguous because the two sessions are independent despite having the same identifier.

Temote already has a separate multi-host architecture in `gateway/` and `temote-mcp gateway-agent`, where routing is explicit by `session_id` and Durable Objects maintain the active host generation. This proposal should preserve that architecture instead of adding implicit cross-host state sharing to direct ingress.

## Problem statement

The current CLI and documentation do not define a safe contract for running `temote-mcp up` on multiple hosts with identical Cloudflare ingress configuration.

The implementation should make the following distinction explicit:

1. **Direct ingress (`temote-mcp up`)**: one externally routable endpoint maps to one Temote host.
2. **Multi-host ingress**: multiple Temote hosts are exposed through the existing gateway-agent / Cloudflare Worker routing plane.

Shared direct-ingress replicas must not be presented as a supported HA mode because Temote session ownership is not replicated or shared.

## Proposed design

### 1. Define host identity

Add a stable host identity for diagnostics and remote introspection.

Suggested configuration:

```text
TEMOTE_MCP_HOST_ID=ubuntu1
```

Requirements:

- host ID is stable across process restarts;
- host ID is not a credential;
- host ID is included in diagnostic/status information returned by the HTTP server and/or session listing metadata;
- defaulting to the OS hostname is acceptable for local diagnostics, but production direct ingress should support an explicit configured value;
- changing host ID must not rewrite or merge existing session metadata.

This identity is primarily observability metadata. It must not be used as a substitute for gateway routing or authorization.

### 2. Make direct ingress single-host by contract

Document and enforce where practical:

```text
one public direct MCP endpoint
        -> one temote-mcp up host
        -> one local session supervisor
```

For Cloudflare direct ingress, each concurrently active host must use a host-specific public endpoint/tunnel configuration.

Example:

```text
ubuntu1.temote.example.com -> ubuntu1
ubuntu2.temote.example.com -> ubuntu2
```

Using the same Tunnel token/hostname concurrently from multiple hosts is unsupported for direct Temote ingress because Cloudflare replica routing is not Temote-session-aware.

### 3. Use the existing gateway for multi-host

The supported single-endpoint multi-host topology remains:

```text
ChatGPT
  |
  v
Cloudflare Worker / Durable Objects gateway
  |
  +-- session_id -> ubuntu1 gateway-agent -> local supervisor
  |
  +-- session_id -> ubuntu2 gateway-agent -> local supervisor
```

Do not add cross-host session replication to `temote-mcp up`.

Do not rely on Cloudflare Tunnel replica selection or HTTP stickiness to preserve Temote session affinity.

The existing gateway generation/lease mechanism remains the authority for which host owns a remotely exposed session.

### 4. Prevent ambiguous operator configuration where possible

Add preflight/doctor checks that can detect unsafe or suspicious direct-ingress configuration without requiring secret disclosure.

At minimum:

- `doctor --profile cloudflare` should print the configured `host_id` and public endpoint;
- documentation must warn that reuse of one direct Tunnel configuration across hosts is unsupported;
- `up` startup output should identify itself as a direct single-host ingress and print the host ID;
- if Temote can reliably derive a non-secret Tunnel identifier from local configuration, include it in diagnostics so operators can compare hosts;
- do not attempt to parse or expose the raw Tunnel token merely to perform this check.

If reliable cross-host duplicate detection cannot be done locally, fail-safe documentation and explicit topology modes are preferable to a heuristic that can produce false assurance.

### 5. Keep host-local session IDs unchanged

Within a direct host, session IDs remain locally unique as today.

For gateway mode, a remotely routable session remains identified by the gateway routing key. Host ID may be added to registry/debug output, but callers should not need to construct composite `host_id/session_id` identifiers if the gateway already provides an unambiguous session route.

If future requirements allow the same `session_id` to be active on multiple hosts simultaneously, that should be a separate protocol change rather than an implicit consequence of direct Tunnel replicas.

## Non-goals

- shared filesystem/session metadata between Temote hosts;
- active-active replication of a running session runtime;
- transparent failover of non-idempotent tool calls between hosts;
- using Cloudflare Tunnel replicas as a Temote session load balancer;
- replacing the existing Durable Objects gateway protocol.

## Acceptance criteria

- [x] documentation explicitly states that `temote-mcp up` direct ingress is single-host per public endpoint
- [x] documentation explicitly states that sharing one Cloudflare Tunnel direct-ingress configuration across multiple Temote hosts is unsupported
- [x] supported multi-host guidance points to `temote-mcp gateway-agent` + Worker/Durable Objects gateway
- [x] a stable non-secret host ID is available in runtime diagnostics
- [x] `temote-mcp up` startup diagnostics show the host ID and direct-ingress topology
- [x] `doctor --profile cloudflare` shows enough non-secret ingress identity information to compare two hosts safely
- [x] no raw tunnel token is logged or returned
- [x] local PID files, supervisor sockets, and session metadata remain host-local
- [x] existing single-host `temote-mcp up` behavior remains compatible
- [x] gateway-agent generation/lease routing remains unchanged
- [x] tests cover host identity parsing/defaulting and diagnostic output where practical
- [x] README / operational documentation is updated in English and Japanese

## Required tests

1. explicit host ID is loaded and reported without affecting session identity
2. missing explicit host ID falls back to the documented local identity behavior
3. malformed/unsafe host IDs fail closed if validation is introduced
4. startup/doctor diagnostics never expose the raw Cloudflare Tunnel token
5. existing direct single-host Cloudflare profile tests continue to pass
6. gateway routing tests remain unchanged and pass

## Follow-up consideration

A later HA design could allow two physical hosts to serve equivalent workloads, but that requires explicit Temote-level placement, fencing, generation ownership, and retry semantics. It should be designed above the session supervisor layer rather than inferred from Cloudflare Tunnel replica behavior.


## Completion evidence — 2026-09-01

Added non-secret host identity through `TEMOTE_MCP_HOST_ID`, with validated OS-hostname fallback. Host ID is reported by supervisor/session diagnostics; `temote-mcp up` identifies direct ingress as `topology=single-host`; `doctor --profile cloudflare` reports host ID, public endpoint, and optional non-secret Tunnel ID without reading or returning the raw Tunnel token. Direct-ingress state remains host-local and gateway generation/lease routing is unchanged. README and managed-session operational docs now state that one direct public endpoint maps to one Temote host, that shared Cloudflare Tunnel replicas are unsupported for sessionful direct ingress, and that multi-host single-endpoint deployments must use `gateway-agent` + Worker/Durable Objects. Host-ID validation/fallback tests and the existing Cloudflare/gateway regression suite pass.
