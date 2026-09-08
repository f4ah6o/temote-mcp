# Multi-host federation: one Temote MCP endpoint for macOS, Linux, and Windows/WSL2 hosts

Status: Done
Closed by triage: 2026-09-08

## Resolution

The federation implementation is complete and merged through PR #11 (`feat: add multi-host federation`).

Delivered behavior includes host-level `gateway-agent --host-id`, leased host discovery via `host_list` / `host_info`, host-aware session lifecycle routing, duplicate session IDs across hosts, fail-closed ambiguous unqualified routing, per-host credential binding, generation fencing, no automatic replay of ambiguous/non-idempotent operations, named-root-name-only host metadata, supervisor protocol compatibility checks, and coexistence with legacy per-session gateway agents.

PR #11 also incorporated review hardening for registry/discovery failures and supervisor rechecks before dispatch. The merged implementation passed Rust formatting/tests/clippy/no-default-features checks, gateway tests, and diff checks.

The original detailed design and acceptance criteria remain available in repository history before this triage archival change.

## Remaining live evidence

Physical macOS/Linux/Windows-WSL2 multi-host acceptance is consolidated into `issues/open/20260908-live-acceptance-matrix.md` and does not keep this implementation issue open.
