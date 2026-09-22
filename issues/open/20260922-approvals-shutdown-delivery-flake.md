# Approvals noprop shutdown delivery timeout

## Status

open — observed once on ubuntu-latest CI, cleared on rerun; watch for recurrence.

## Evidence

- PR #17 run `35750203797`, job `106822053342` (`rust (ubuntu-latest)`) at `1e287f1`:
  `approvals::tests::generated_shutdown_denies_all_pending_approvals` failed with
  `pending approval was not delivered: Elapsed(())` (noprop seed
  `0x15151d1d070d5555`, case_index 55).
- Same commit passed `rust (ubuntu-latest)` on rerun `35751523952`, job
  `106826594707`. macOS job passed both times.
- 1056 tests passed in the failing run; only this generated case failed.

## Hypothesis

The generated shutdown case races the approval broker's delivery loop against a
wall-clock timeout; under CI runner load the delivery can arrive after
`Elapsed` fires. No product change is proposed from a single observation — the
delivery is correct when timing is unpressured.

## Acceptance

- If the same test fails again in CI, collect the RunError seed and decide
  whether the delivery timeout needs a load-tolerant bound or whether the
  broker ordering has a real gap.
- Close without action if it does not recur.
