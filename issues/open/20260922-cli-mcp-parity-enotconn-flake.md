# cli_mcp_active_session_parity ENOTCONN transient failure

## Status

doing — recurrence confirmed on macos-latest CI; fix implemented on `fix/20260926-macos-session-flakes` and awaiting CI verification.

## Evidence

- PR #17 run `35746553811` at `9a7d5a5`: `cli_mcp_active_session_parity` failed
  on macos-latest with a transport-level `ENOTCONN` while reading the active
  session over the MCP socket.
- The same test passed on the same workflow's rerun and in subsequent green
  runs (jobs `106822051876`, `106826594623`).
- Main CI run `36206714233`, macos-latest job `108304685583` on
  2026-09-26: the same test recurred while starting a named session:
  `Error: Socket is not connected (os error 57)`.
- Main CI run `35901733559`, macos-latest job `107319366832` on
  2026-09-23: the same test recurred with a parity mismatch where the CLI view
  still contained `parity-a` / `parity-b` but the following MCP view only
  contained `parity-c`.

## Hypothesis

The parity check polls or connects to the supervisor's Unix socket while the
session is still establishing; on a loaded macOS runner the connect can race
the listener becoming ready, surfacing `ENOTCONN` from the client side. No
product gap was identified from a single observation.

## Acceptance

- If it recurs, capture whether the supervisor had bound the socket before the
  client connect attempt, and consider a bounded retry in the fixture rather
  than a product change.
- Close without action if it does not recur.

## Recurrence diagnosis and mitigation (2026-09-26)

The recurrence happened after the fixture's `wait_for_supervisor()` had already
completed a successful `session list`, so the supervisor socket was bound and
had answered a control request before the later failure. The control connect
path adds its own context on failure, while the CI error was the bare macOS
`ENOTCONN`; the uncontextualized post-write `stream.shutdown().await?` in
`request_at_path` was therefore the matching failure point.

The client now treats only `ErrorKind::NotConnected` from that post-write
half-close as benign and continues to the mandatory response read. It does not
replay the request, which avoids duplicating a possibly-applied `session start`
or other mutation. Other half-close failures still fail. A focused unit test
covers that classification.

The retention/session E2E fixture also waits, with a bounded deadline, until a
newly-started session is observable as `active` before parity assertions. This
addresses the separate establishment race seen in run `35901733559` without
weakening the parity assertion.
