# cli_mcp_active_session_parity ENOTCONN transient failure

## Status

open — observed once on macos-latest CI, cleared on rerun.

## Evidence

- PR #17 run `35746553811` at `9a7d5a5`: `cli_mcp_active_session_parity` failed
  on macos-latest with a transport-level `ENOTCONN` while reading the active
  session over the MCP socket.
- The same test passed on the same workflow's rerun and in subsequent green
  runs (jobs `106822051876`, `106826594623`).

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
