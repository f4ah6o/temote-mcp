# S1: single backend-neutral source for the task report schema

Status: ready
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `c305e41`
Parent issue: `issues/open/20260927-native-structured-output-agent-backends.md`
Prerequisites: none (refactor-only slice; no wire change)

## 1. Goal

The Temote task report contract (field list, bound constants, validator, and the
JSON Schema sent to backends that support native structured output) is produced
from exactly one backend-neutral module. Per-backend copies stop drifting.

## 2. Fixed decisions

- One canonical module (new `src/report_contract.rs`, or a clearly named
  section in `src/delegation/mod.rs`) owns:
  - the required field list (currently `REPORT_FIELDS` at
    `src/delegation/mod.rs:25`),
  - the bound constants (`MAX_REPORT_*` at `src/delegation/mod.rs:37-41`,
    `MAX_SUMMARY_CHARS`/`MAX_REPORT_ARRAY_ITEMS`/`MAX_REPORT_BYTES` at
    `src/opencode_server.rs:43-45` and their devin counterparts),
  - `validate_report_schema` (`src/delegation/mod.rs:1021`) as the shared
    validator,
  - a `report_json_schema()` producer equivalent to today's
    `devin_cloud::report_schema()` (`src/devin_cloud.rs:85`).
- `devin_cloud::report_schema()` is replaced by the shared producer; the
  serialized JSON Schema sent to Devin API v3 must be byte-identical to the
  current output (pin with a snapshot test).
- `opencode_server::report_shape_valid` (~line 3377), `devin_acp`'s copy of the
  same validator, and `delegation`'s `ReportState` checks all delegate to the
  shared validator. Where bounds differ today (e.g. 4 KiB vs 8 KiB report caps),
  keep each backend's existing bound — do not silently unify limits; document
  any discovered divergence in the parent issue.
- No backend gains or loses a field; public `report` object shape is unchanged.

## 3. Read / change scope

- `src/delegation/mod.rs`: `REPORT_FIELDS`, `MAX_REPORT_*`,
  `validate_report_schema`, `ReportState`.
- `src/devin_cloud.rs`: `report_schema()` (line 85), `report_shape_valid`
  (line 1483), `structured_output_schema` callsite (line ~2246).
- `src/opencode_server.rs`: `report_shape_valid` (~line 3377), bound consts
  (lines 43-45), `REPORT_INSTRUCTIONS` (line 112) stays as prompt text.
- `src/devin_acp.rs`: `extract_report`/`report_shape_valid` (lines ~2724+).
- `src/codex_app_server.rs`: locate its report validation path and route it to
  the shared validator; note whether it currently has one at all (the parent
  issue flags this gap — finding it is part of the packet).
- `src/main.rs`: register the new module if a new file is introduced.

## 4. Steps

1. Diff the four validator/schema implementations; table the divergences in the
   parent issue before writing code.
2. Move the shared contract to one module; re-export only what callers need.
3. Migrate each backend's callsite; snapshot the Devin JSON Schema output.
4. Add a parity test asserting the validator accepts/rejects identical fixtures
   across backends.
5. Focused tests; `just sandboxed-check`; host gates recorded as NOT RUN.

## 5. Acceptance

- [ ] Exactly one module defines the report field set and its validator.
- [ ] Devin Cloud emits a `structured_output_schema` identical to the pinned
      snapshot (no wire change).
- [ ] OpenCode serve / Devin ACP / legacy delegation validators reject and
      accept the same fixtures.
- [ ] Bound differences between backends, if any remain, are explicit constants
      documented in the parent issue — not accidental drift.
- [ ] `cargo test --locked`, clippy, fmt, no-default-features check, and
      `git diff --check` are clean.

## 6. Validation commands

- `cargo test --bin temote-mcp --all-features --locked report`
- `cargo test --bin temote-mcp --all-features --locked delegation`
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `(cd gateway && npm test)` — only if the wire surface changed; this packet
  should produce a zero-diff contract snapshot.

## 7. Delivery authorization

One feature branch + one PR to `main`. Refactor only; behavior parity is the
acceptance bar.

## 8. Completion report

(to be filled by the implementing packet run)
