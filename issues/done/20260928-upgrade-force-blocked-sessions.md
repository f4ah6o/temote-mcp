# upgrade --dry-run の NG session 一覧と upgrade --force による復元不能 session の切り離し

Status: Done
Closed by triage: 2026-09-28

## Resolution

`upgrade --dry-run` now lists every session the upgrade cannot restore as `blocked_sessions` (session id + reason), and `upgrade --force` pushes the handoff through by stopping those sessions.

Implemented behavior: preflight compatibility gates (direct ingress, helper generation) and executable revalidation run before any session is stopped; each forced stop verifies the live session still matches the previewed instance (a session restarted after the preview reuses the id but is never stopped); a failed stop aborts the upgrade before handoff; and after the stops the preflight and all gates re-run against the fresh state. Stopped sessions are not restored and remain `degraded`.

Landed via PR #80 (`3947fc2`), including the process-boundary E2E `supervisor_upgrade_force_stops_unrestorable_sessions`, the negative gate E2E `supervisor_upgrade_force_leaves_blocked_sessions_when_gates_fail`, and unit tests covering fatal stop failure and replaced/unverifiable skip paths. Documentation updated in `docs/managed-sessions{,.ja}.md`, `docs/usage{,.ja}.md`, `skills/temote-mcp/SKILL.md`, and `CHANGES.md`; the live-test recipe was added to `.agents/skills/testing-temote-mcp/SKILL.md` via PR #82.

The original detailed design, acceptance checklist, and evidence remain available in repository history before this triage archival change.

## Follow-up

Out of scope and intentionally unchanged: the remote `upgrade_apply` path remains strict (approval-based, no force semantics); non-force dry-run still reports blockers only when a handoff is required; and durable metadata of stopped sessions is kept (a `session stop`, not `forget`).
