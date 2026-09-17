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
