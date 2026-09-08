# Zero-downtime supervisor upgrade / handoff

Status: Done
Closed by triage: 2026-09-08

## Resolution

The Level B coordinated supervisor handoff path is implemented and all acceptance criteria in the original issue are satisfied.

Implemented behavior includes target capability/version/protocol validation, lifecycle mutation fencing, non-secret restart planning, same-PID `exec` supervisor handoff, active-session restore and socket verification, deterministic restore failure reporting, direct-ingress coordination and health verification, dry-run planning, idempotent same-version handling, and Codex plugin reconciliation.

Representative implementation commits include `f9485873` (`feat: add safe supervisor upgrade handoff`), `1d8183a8` (`feat: report supervisor upgrade restore failures`), `1af6bc28` (`feat: report dry-run upgrade blockers`), `c86ee401` (`feat: coordinate direct ingress upgrades`), and `818be1dd` (`test: cover supervisor upgrade generations`).

The original detailed design, acceptance checklist, and evidence remain available in repository history before this triage archival change.

## Follow-up

Remote MCP-initiated upgrade/reconnect semantics are intentionally separate and remain tracked by `issues/open/20260908-07-client-safe-upgrade-reconnect.md`.
