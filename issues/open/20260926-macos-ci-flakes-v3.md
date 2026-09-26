# macOS CI flakes observed during V3 VCS adapter work

Status: open  
Repository: `f4ah6o/temote-mcp`  
Observed: 2026-09-26 (Asia/Tokyo)  
Related PR: #59 (`feat: add jj-first VCS transaction core`)

## Summary

During V3 VCS adapter validation, two existing macOS tests failed independently of the VCS change and passed on immediate job rerun.

The V3 diff was limited to `src/vcs.rs` and the `mod vcs` declaration in `src/main.rs`. Both failing tests are outside that surface. Ubuntu full CI passed throughout.

These failures increase friction because a feature branch can be green on all changed-code tests while the required macOS job remains red until manually rerun.

## Flake A — Codex app-server restart drain

Observed failure:

```text
codex_app_server::tests::restart_does_not_start_replacement_until_old_turn_drained
src/codex_app_server.rs:4615:58
called Result::unwrap() on an Err value: Broken pipe (os error 32)
```

Behavior:

- initial macOS job: FAIL
- same job rerun, same feature SHA: PASS
- no change to `codex_app_server.rs` between failure and success

Confirmed cause: **unknown**.

Likely investigation area, not yet confirmed:

- test child/process shutdown ordering
- writer attempts after peer close
- a race between old-turn drain completion and replacement-session startup

Do not paper over the failure with unconditional retries inside production code.

## Flake B — session metadata bounded listing

Observed failure:

```text
session_list_remains_bounded_and_deterministic
tests/session_metadata_retention_e2e.rs:576
assertion left == right failed
```

The result contained one additional historical entry:

```text
left:  ... "bounded-00515", "bounded-00514", "bounded-00513"
right: ... "bounded-00515", "bounded-00514"
```

Behavior:

- initial macOS job on the latest-main-synced V3 SHA: FAIL
- immediate macOS job rerun on the same SHA: PASS
- VCS unit tests in the same failed job: PASS
- Ubuntu full CI on the same SHA: PASS

Confirmed cause: **unknown**.

Likely investigation area, not yet confirmed:

- boundary/count selection around active sessions versus retained historical entries
- filesystem/metadata ordering at the scan limit
- test assumption that an exact cutoff remains deterministic under macOS timing/filesystem behavior

## Acceptance for a fix

### Codex restart test

- [ ] repeated macOS execution does not produce `Broken pipe`
- [ ] test still proves a replacement session cannot begin before the old turn drains
- [ ] production failure semantics are not weakened merely to make the test pass

### Session metadata retention test

- [ ] repeated macOS execution produces the same bounded list
- [ ] active-session priority remains guaranteed
- [ ] historical cutoff is defined explicitly rather than depending on incidental filesystem iteration/timing
- [ ] Linux behavior remains unchanged

## Suggested validation

Run the two focused tests repeatedly on macOS before and after a fix, then run the normal full CI matrix.

Do not treat a single rerun PASS as proof that either underlying race is fixed.
