# `just sandboxed-check` runtime boundary: pure gate vs host/CI-only acceptance

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/doing/20260916-normal-session-ci-sandbox-friction.md`
Depends on: none

## Current code and contract

`just sandboxed-check` is documented as the deterministic subset that runs from inside an
already-sandboxed normal Temote `agent` session (`permission_mode=agent`, `yolo=false`). The
2026-09-17 independent review measured these failures in that environment:

- `session_control::tests::session_gc_*` (5 tests): `UnixListener::bind` fails
  `Operation not permitted (os error 1)` (src/session_control.rs:6287), and
  `session_gc_grace_period_boundary_is_respected` also finds no candidate because
  `session_gc_orphan_is_safe` -> `config::session_is_active` fails closed on the socket probe;
- `npm test --prefix gateway` fails in `deployment-preflight.test.mjs`: the nested
  `spawnSync(process.execPath, ...)` returns `EPERM`, while the same CLI run via a parent Temote
  `execute` produces the expected JSON/exit status;
- the `local_agent` real-wiring tests need a nested Linux sandbox and report
  `temote-linux-sandbox: bubblewrap is required for Linux sandboxing`.

The recipe itself selects the socket-dependent `session_gc` tests and the full gateway suite, so it
cannot pass in the environment it is documented for.

## The one responsibility to change

Separate the sandbox-compatible gate from host/CI-only acceptance without deleting tests or
weakening coverage:

- move the five socket-liveness `session_gc` tests into
  `session_control::tests::host_liveness_tests` (host runs them via `cargo test`/CI) and keep the
  pure policy tests (`session_gc_metadata_diagnostics_explain_orphan_classes`,
  `session_gc_limit_is_bounded`) in the sandboxed selection;
- add `gateway` `test:sandbox` (`node --test test/protocol.test.mjs`) for the evaluator tests and
  keep `npm test` (all tests, including `deployment-preflight.test.mjs`) as the host/CI gateway
  gate;
- `just sandboxed-check` uses the sandbox-compatible gateway recipe and prints every host-only
  suite as an explicit `NOT RUN` line;
- `just check`, CI, and `just linux-sandbox-acceptance` keep the full coverage.

No test is deleted, no assertion is ignored, and `NOT RUN` is never reported as PASS.

## Not changing

- Product sandbox or socket behavior.
- CI workflow selection: the full suite still runs on the unsandboxed runner.
- macOS native tests remain a macOS/CI gate.

## Verification

- in a normal Temote agent session: `just sandboxed-check` exits 0 and prints the host-only
  `NOT RUN` lines;
- on the host: `just check` still runs the full `cargo test` (including `host_liveness_tests`) and
  the complete gateway suite;
- host: `just linux-sandbox-acceptance` still passes.

## Completion condition

`just sandboxed-check` passes from inside a normal Temote agent session with an explicit host-only
`NOT RUN` list, and host/CI coverage is unchanged.

## Implementation notes (2026-09-17)

Changes:

- `src/session_control.rs`: the five socket-liveness tests moved to
  `session_control::tests::host_liveness_tests` with a module comment; the pure
  `session_gc_metadata_diagnostics_explain_orphan_classes` and `session_gc_limit_is_bounded` stay in
  `tests`.
- `gateway/package.json`: added `test:sandbox` (`node --test test/protocol.test.mjs`); `npm test`
  still runs the complete suite including `deployment-preflight.test.mjs`.
- `justfile`: `sandboxed-check` now runs `session_control::tests::session_gc` (pure), the
  sandbox-compatible gateway recipe, and prints explicit NOT RUN lines for session-GC socket
  liveness, `sandbox::linux_tests`, local-agent real-wiring, deployment-preflight subprocess tests,
  full binary/socket integration, supervisor E2E, and macOS Seatbelt. `just check`/CI keep the full
  coverage; `gateway-test` remains the complete host suite.
- `AGENTS.md`, `docs/development.md`: boundary text updated to name the moved suites.

Measured:

- host `just sandboxed-check`: exit 0 (lib 113, agent_git 27, activity_job 6, activity_coverage 3,
  upgrade_transaction 40, upgrade_coordinator 6, pure session_gc 2, gateway sandbox 64, clippy /
  no-default / fmt / diff clean) and the seven NOT RUN lines above.
- host `cargo test` still runs `host_liveness_tests` 5/5 PASS.
- host full gateway suite (`npm test`) is part of `just check`/CI and unchanged.
- inside a real normal Temote agent session: NOT RUN locally (this worker is on an unsandboxed X11
  host); the review session is the acceptance environment for requirement A.

## 2026-09-17 completion

The repair round above passed independent review in the documented normal Temote agent session;
`just sandboxed-check` stays the deterministic gate, and the explicit host/CI-only rows remain
NOT RUN there by design.
