# Git broker response publication and indeterminate outcomes

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: `issues/polished/20260917-git-broker-queue-filesystem-boundary.md`

## Current code and contract

- Requests are staged as `*.tmp` then renamed (good).
- Responses are written directly to the final path (`src/agent_git.rs:336`), so a reader can observe
  a partial JSON document; the direct write also follows a planted symlink (Packet D).
- The client reads the response, deletes response and request, and only then decodes
  (`src/agent_git.rs:448`), so cleanup happens before the result is known.
- On timeout the client deletes the queued request and reports the generic rejection message
  (`run_shim` -> `reject()`), even though the broker may already be executing the mutation; a retry
  can double-apply.
- `GitBroker::drop` aborts the serve loop and removes the queue; an in-flight operation is neither
  cancelled nor reported.

## Reproduction

- Make the broker's response write observable mid-write (large payload, `strace`, or a small
  wrapper): a concurrent reader can parse truncated JSON.
- Kill/stop the run while a slow `commit` is executing: the shim prints the rejection text, which an
  operator can misread as "nothing happened".

## The one responsibility to change

Make every outcome explicit and non-ambiguous:

- broker publishes responses with the Packet D atomic temp+rename primitive; readers never see
  partial JSON;
- shim outcome is a typed enum: completed result / rejected / indeterminate; only a broker error
  payload maps to the existing rejection message and exit `128`;
- timeout, read failure, undecodable or wrong-schema response, and missing queue map to a distinct
  indeterminate outcome with its own fixed message and exit code, explicitly telling the caller not
  to retry automatically;
- cleanup happens only after a successfully decoded response; an indeterminate request is left in
  place (the operation may still be executing), not re-labeled as "not executed";
- broker shutdown does not claim in-flight operations were not applied.

## Not changing

- No automatic retry is added anywhere.
- Request/response schemas, timeout durations, and the fixed rejection message stay.
- Structured Git tools and their result reporting are untouched.

## Focused tests

- barrier-ordered test: client is blocked at read while broker publishes; client only observes a
  complete document.
- read/decode failure produces the indeterminate message and never the rejection text.
- timeout produces the indeterminate message; response files are not deleted before decode.
- broker-rejected request still produces the rejection message and exit 128.
- completed request still returns the Git status/stdout/stderr verbatim.

## Host / CI / provider verification

- Linux host/CI focused tests; real agent-observed shim behavior is a live-matrix row.

## Completion condition

No partial response is observable, no indeterminate mutation is reported or retried as a rejection,
and focused tests plus `just sandboxed-check` pass.

## Implementation notes (2026-09-17)

Changes in `src/agent_git.rs`:

- `write_entry_atomic` publishes every response through an exclusive temp entry and `renameat`.
- `ShimOutcome { Completed(ShimResult), Rejected, Indeterminate }`; only a decodable broker error
  payload maps to `Rejected` (exit 128). Timeout, unreadable/undecodable/wrong-schema responses,
  and a missing queue map to `Indeterminate` (exit 70, fixed message that says the operation may
  still be running and must not be retried automatically).
- Cleanup happens only after a successfully decoded response; an indeterminate request is left in
  the queue, so a timeout is never re-labeled as "not executed".
- `GitBroker::drop` still aborts the serve loop; a client waiting on an in-flight operation now sees
  the indeterminate outcome instead of the rejection message.

Focused tests (PASS):

- `published_responses_are_never_partially_visible` — a reader thread racing five atomic publishes
  observes only complete payloads (or nothing).
- `shim_outcome_distinguishes_rejection_from_indeterminate` — queue without a broker times out as
  `Indeterminate` and leaves the request file queued.
- `shim_and_broker_round_trip_over_the_private_directory` — completed result verbatim and broker
  rejection still exit through the rejection path.
- `run_shim_routes_mutations_to_the_broker_and_read_only_commands_to_git` covers the routing enum.

NOT RUN: real agent observing exit 70 live; macOS host execution.

## 2026-09-17 correction

Response cleanup moved to the broker/parent: the sandboxed shim can no longer delete response
files (they live in a read-only root). The `Completed`/`Rejected`/`Indeterminate` semantics and
atomic publish are unchanged; see
`issues/polished/20260917-git-broker-response-authority.md`.
