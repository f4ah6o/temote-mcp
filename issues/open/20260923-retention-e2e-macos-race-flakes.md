# session_metadata_retention_e2e macOS transient failures

## Status

open — two distinct failures observed on macos-latest CI on 2026-09-23,
each clearing on the sibling/retried run of the same commit.

## Evidence

- PR #23 run job `106982530286`: `session_list_remains_bounded_and_deterministic`
  failed on macos-latest; two consecutive `session_list` reads returned
  different session id lists (`first_ids != second_ids`), while 600 seeded
  terminal pairs were being materialized. Passed on retrigger and locally.
- Main `CI` run `35800012736` job `106987986940` at `168b53f`:
  `session_gc_dry_run_and_apply_remove_only_old_orphans_and_survive_restart`
  failed on macos-latest at `session_metadata_retention_e2e.rs:868`,
  `run_cli(&["session", "list"]).status.success()` — a CLI invocation could
  not complete against the live supervisor. The sibling `Push on main` run
  of the same commit passed.

## Hypothesis

Both assertions involve a fresh CLI/MCP client reaching the supervisor while
fixture state is settling (metadata materialization and post-GC scans). On a
loaded macOS runner the client connect/read can race the supervisor, in the
same connect-race family as `20260922-cli-mcp-parity-enotconn-flake.md`. No
product gap was identified from single observations; ubuntu-latest and local
runs are consistently green.

## Acceptance

- If either recurs, capture the CLI stderr for the failing `run_cli` call
  (the assert currently drops it) and check whether the supervisor socket was
  bound before the connect; fix via a bounded fixture retry rather than a
  product change.
- Close without action if neither recurs.
