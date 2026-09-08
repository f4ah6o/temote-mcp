# Cloudflare Workers + Durable Objects によるサーバレス gateway を実装する

Status: Done
Closed by triage: 2026-09-08

## Resolution

The Worker / Durable Object gateway, endpoint agent, documentation, and repository-local tests are complete. The implementation was committed to main beginning with `74d1de4` (`feat: add Cloudflare Durable Object gateway`) and later became the foundation for host-aware federation.

Completed behavior includes single MCP endpoint routing, registry/session Durable Objects, outbound host polling, generation fencing, stale-response rejection, terminal approval before gateway connection, Mac/Linux/WSL2 agent support, and preservation of sandbox/approval boundaries.

The original detailed design and acceptance evidence remain available in repository history before this triage archival change.

## Remaining live evidence

Real Cloudflare deployment and concurrent external session acceptance are consolidated into `issues/open/20260908-live-acceptance-matrix.md`.
