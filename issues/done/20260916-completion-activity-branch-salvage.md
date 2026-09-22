# Salvage only missing activity work from 2026-09-15 completion branches

Status: done
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/doing/20260914-local-activity-viewer.md`
Depends on: Phase 1 repository-local friction packets
Source branch: `codex/20260915-completion-activity`

## Goal

Compare current `main` semantically with the old completion-activity branch and port only still-missing S05-S16 behavior. Do not merge the branch.

## Work packet

1. Build a checklist mapping S05-S16 requirements to current-main symbols/tests/docs.
2. Mark each requirement `already-covered` or `missing` with file/test evidence.
3. For the smallest coherent missing behavior, port it to current main and add focused tests.
4. Repeat as separate commits/iterations; do not import old unrelated issue/doc state.
5. After all missing behavior is covered, run activity-focused tests + gateway if protocol changed + `just sandboxed-check`.

## Acceptance

Parent issue gets a concise current-main completion table. Old branch is then safe for Phase 5 deletion after independent diff review.

## Flash-sized execution rule

This packet is **audit/reconciliation only** for S05-S16. Do not port multiple missing behaviors in this task. Produce a current-main coverage table with exact files/tests/old commits. For every missing coherent behavior, create one new `issues/polished/` child issue with a single change scope and exact focused tests. Parent Temote reviews that issue list before implementation.

## Implementation

2026-09-22 audit, no code ported. The audit compared `codex/20260915-completion-activity` (tip `112b3cd`, carrying `5be7782`, `8a6efff`) with current `main` file-by-file over `src/activity/`, `src/activity_runtime.rs`, `src/session_control.rs`, `src/supervisor.rs`, `src/approvals.rs`, `src/mcp.rs`, `src/cli.rs`, `src/main.rs`, docs, and CHANGES.md. Every S05-S16 requirement is already covered on `main`; the per-unit evidence table was appended to the parent as `## 2026-09-22 current-main coverage (salvage audit)`. Current `main` is a strict superset of the branch's activity surface (it adds `GitWorktreeCreate`, `GitRemoteBranchDelete`, `GithubPr*`, `GithubWorkflow*`, `Opencode*`, `SessionPermissionGrant/Ungrant`, `SandboxSetupFailed` operations the branch never had). The only branch-only deltas are the completion evidence documents under `docs/completion-*` and `issues/open/20260908-live-acceptance-matrix.md` edits (owned by `issues/polished/20260916-completion-evidence-branch-salvage.md`) plus stale app-server/issue state owned by other packets — none of it is S05-S16 behavior.

Missing coherent behaviors: none. No child issues were created. `codex/20260915-completion-activity` is safe for Phase 5 deletion after independent diff review.

## Verification

- `git diff main origin/codex/20260915-completion-activity -- src/activity*` shows only main-side additions (superset); `src/activity.rs`, `history.rs`, `broker.rs`, `scope.rs` are identical.
- Parent checklist rows S05-S15 verified against current-main symbols/tests; S16's English/Japanese docs and `CHANGES.md` entry confirmed at `docs/usage.md:18`, `docs/usage.ja.md:18`, `docs/managed-sessions.md` (Local activity viewer), `docs/managed-sessions.ja.md`, `CHANGES.md`.
- `git diff --check` clean; issue-only change, no Rust/gateway gates apply.
