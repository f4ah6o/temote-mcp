# `session_list` aborts when a supervisor-owned session working directory is gone

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/doing/20260908-07-client-safe-upgrade-reconnect.md` (lifecycle surface)
Depends on: none

## Observation (2026-09-17 independent review)

A `session_list` call from current `main` failed entirely:

```text
failed to inspect supervisor-owned session temo-activity:
cannot resolve /home/hirohito-fujita/src/temote-completion-activity-20260915:
No such file or directory
```

`session_info` for the active session `temo` still worked. The failing session's working
directory was a completion worktree that had already been removed; the session metadata remained
supervisor-owned.

## Current code and contract

`list_session_views` (`src/session_control.rs:3564`) calls `inspect_session_read_only(id)` for
every supervisor-owned id and propagates any error with `with_context(...)`, so one stale owned
session aborts the whole listing. History sessions are already tolerated
(`if let Ok(session) = inspect_session_read_only(&id)`), and `session_metadata_diagnostics` /
retention handle missing halves without failing the scan.

This case is not owned by:

- `issues/done/20260905-04-session-metadata-retention.md` (bounded scan / retention);
- `issues/done/20260911-session-forget-stale-metadata.md` (explicit `session forget`);
- `issues/done/20260916-session-orphan-gc.md` (missing metadata-half GC classes);
- the managed worktree/branch issues (worktree lifecycle only).

## The one responsibility to change

Decide and implement the supported behavior for a supervisor-owned session whose cwd no longer
resolves, without deleting session or worktree state:

- `session_list` must remain usable when one owned session's workspace is missing; either report
  that session with a bounded degraded status/field or skip/report it as an explicit entry, but do
  not fail the whole list;
- keep `session_info` behavior explicit for the same case;
- no raw host paths beyond what the session view already returns;
- no implicit deletion or `forget` from the read path.

## Not changing

- Ownership/liveness semantics, retention/GC, and explicit `session forget`.
- No cleanup of the stale session or its metadata in this packet.

## Verification

- focused tests: owned session with a deleted cwd + a healthy owned session; listing still returns
  the healthy session and a bounded representation of the stale one; `session_info` for the stale
  id stays non-panicking;
- regression: existing session_list/retention E2E stays green;
- host/CI: full binary suite; live re-check with the real `temo-activity` metadata is a live-matrix
  row (do not delete the record to test this).

## Completion condition

A missing workspace on one owned session cannot take down `session_list`, and the behavior is
covered by a focused regression test.

## Implementation notes (2026-09-17)

Changes:

- `src/config.rs`: `read_session_metadata` stays strict for execution loaders; the loader and
  identity checks were split out, and `SessionViewMetadata` + `read_session_metadata_for_view`
  serve read-only views. `validate_loaded_session_for_view` keeps the ID/identity, non-empty-root,
  duplicate-root, and cwd-membership invariants and returns whether every stored workspace path
  still resolves. `session_path_resolves` requires an absolute, normalized path that canonicalizes
  to itself and is a directory, and only tolerates `NotFound`/`NotADirectory`;
  `verify_existing_ancestor_is_canonical` then fails closed when an existing ancestor is a broken
  symlink, resolves elsewhere, or is not a directory.
- `src/session_control.rs`: `build_session_view(id, reconcile_lifecycle)` is shared by
  `inspect_session` (lifecycle reconciliation may persist a crash) and `inspect_session_read_only`
  (never writes). An unresolvable workspace reports `status="degraded"` with a bounded `last_error`
  and keeps the stored id/cwd/roots/timestamps; it is never relabeled active/stopped/crashed, and
  `pid` is only shown when the socket probe observed a live session. Owned sessions are still
  listed when degraded, and `collect_bounded_history_session_ids` excludes metadata whose workspace
  no longer resolves before `MAX_SESSION_HISTORY_CANDIDATES` applies so stale entries cannot crowd
  out healthy history. Stale metadata and lifecycle state are never deleted.
- `src/mcp.rs`: the `session_list` description documents the degraded status, and a regression test
  asserts `session_info` renders a removed workspace as `degraded` without deleting metadata.
- `docs/usage.md`, `docs/usage.ja.md`, `CHANGES.md`: behavior documented.

Focused tests (PASS):

- `config::tests::session_view_metadata_tolerates_a_removed_workspace`
- `config::tests::session_view_metadata_degrades_when_one_permitted_root_is_gone`
- `config::tests::session_view_metadata_rejects_malformed_and_unsafe_metadata`
- `config::tests::session_view_metadata_rejects_symlinked_workspace_paths`
- `session_control::tests::session_list_survives_an_owned_session_with_a_removed_workspace`
- `session_control::tests::degraded_history_is_excluded_before_the_candidate_bound`
- `session_control::tests::missing_workspace_is_never_reported_as_a_liveness_outcome`
- `mcp::tests::session_info_renders_a_missing_workspace_as_degraded`
- `mcp::tests::routed_gateway_contract_matches_checked_in_snapshot`
- `mcp::tests::public_contract_fingerprint_matches_checked_in_snapshot`

Gates:

- `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo check --no-default-features --all-targets --locked`, `git diff --check`: PASS.
- `just sandboxed-check`: exit 0 (lib 113/113 including the config/session_control tests above,
  agent_git 28, activity_job 6, activity_coverage 3, upgrade_transaction 40, upgrade_coordinator 6,
  pure session_gc 2, gateway sandbox 64, clippy/no-default/fmt/diff clean) with the seven
  unchanged host/CI-only NOT RUN lines.
- NOT RUN: full binary/local Unix-socket suite, live re-check with the real `temo-activity`
  metadata (live-matrix row; the stale record was not deleted to test this).

## 2026-09-17 completion

A missing workspace on one supervisor-owned session no longer takes down `session_list`; the
degraded view keeps identity and liveness facts, the execution loader stays strict, and the
independent review approved this packet.
