# session_metadata_retention_e2e macOS transient failures

## Status

doing — both failure families recurred on macos-latest through 2026-09-26;
fixture/transport mitigations are implemented on `fix/20260926-macos-session-flakes` and awaiting CI verification.

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
- Main CI run `36152309200`, macos-latest job `108128336065` on
  2026-09-25: the GC test recurred at `session gc --apply --limit 10` with
  `Error: Socket is not connected (os error 57)`.
- Main CI run `36209547737`, macos-latest job `108313042849` on
  2026-09-26: `session_list_remains_bounded_and_deterministic` recurred; the
  first list still contained `bounded-00513` while the immediately following
  list did not.
- Main CI run `35955228186`, macos-latest job `107492027287` on
  2026-09-24: the deterministic-list failure also recurred.

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

## Recurrence diagnosis and mitigation (2026-09-26)

The bare macOS `ENOTCONN` is handled at the control client's post-write
half-close, not by retrying `gc --apply`. Retrying a mutating request after an
uncertain transport failure could apply the operation twice. The client now
allows only `NotConnected` at that half-close and still requires a valid
supervisor response before reporting success.

For the deterministic-list case, the fixture now waits for the seeded
`bounded-*` terminal metadata to reach the retention contract of 512 complete
`.json` / `.state` pairs before making the two consecutive MCP reads. The
assertion itself remains unchanged. This makes the test prove fixture
quiescence instead of depending on runner timing.

The fixture's active-session helper now uses a bounded read-only `session info`
poll after each successful start, and the post-GC list/info assertions include
both stdout and stderr so any future recurrence preserves the transport
evidence requested by this issue.
