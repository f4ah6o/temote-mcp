# R1: completed task keeps a bounded raw result when the final report is malformed

Status: ready
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `c305e41`
Parent issue: `issues/open/20260927-completed-task-malformed-final-report-json.md`
Prerequisites: none (self-contained fix against current `main`)

## 1. Goal

A task whose backend turn reaches `completed` must not lose its result body when
the final report payload is invalid JSON / schema-invalid / oversized. The caller
can always read the observed terminal state plus a bounded raw output, without
re-running the task.

## 2. Fixed decisions

- Task lifecycle state and report decode state are stored as separate facts:
  - `TaskStatus` (`completed`, etc.) — unchanged semantics;
  - a new report-state field distinguishing `valid` / `invalid_json` /
    `invalid_schema` / `oversized` / `missing` (reuse the vocabulary of
    `ReportState` in `src/delegation/mod.rs:215`).
- Bounded raw final output is preserved when structured extraction fails:
  - store a bounded copy of the final assistant text (truncated with explicit
    `truncated: true` metadata when over the report bound) on the terminal task
    record or in the existing scoped-evidence payload written by
    `store_evidence_for_instance` (`src/opencode_server.rs:1708`), which today
    stores only the parsed `report`.
- `task_get` keeps returning the task with `status: "completed"`; the report
  failure is exposed as report status + raw output + decode error, not by
  rewriting the task status.
- Old task records without the new fields must still load
  (`#[serde(default)]` on every new field).

## 3. Read / change scope

- `src/opencode_server.rs`
  - `TaskRecord` (line ~416): add `report_state` / bounded raw-output fields.
  - `derive` path around line ~3285–3310: today `message_completed` →
    `status: Completed` with `report: None` and only a `last_error` string when
    `extract_report` fails; the raw `text` is dropped. Preserve bounded raw text
    and record the decode classification.
  - `extract_report` (line 3327): extend `(Option<Value>, Option<String>)` to
    also return a machine-readable decode class (or add a sibling function) so
    `invalid_json` / `invalid_schema` / `empty` are distinguishable.
  - terminal evidence payload (line ~3968–3984): include the bounded raw output
    and report state so `evidence_read` can recover it.
  - `task_view` (line ~1670): surface `report_state` (additive key only).
- `src/devin_acp.rs`: same treatment at the `extract_report` callsite
  (line ~2637) — `extract_report` (line 2724) is a copy of the OpenCode shape.
- `src/devin_cloud.rs`: `derive_state` (lines ~1590–1670) already prefers
  `structured_output`; when the session finishes with no valid report
  (`finished(None)`), keep a bounded copy of the message text used by
  `extract_report` (line 1440) instead of only `last_error`.
- `src/delegation/mod.rs`: `ReportState`/`read_report` (lines 215, 981) are the
  legacy-path precedent — align vocabulary, do not change legacy behavior.
- Do not change report schema rules, byte limits, or the public report shape.

## 4. Steps

1. Read the decode → record → `task_get` → evidence chain in the three backend
   files above; find exactly where the raw payload is dropped (each callsite
   where `extract_report` / structured output yields `None`).
2. Add a shared report-decode classification (either reuse
   `delegation::ReportState` names or introduce a small backend-neutral enum).
3. Persist bounded raw output + `report_state` on terminal records; serialize
   with `#[serde(default)]` for backward compatibility.
4. Include the raw output + state in the terminal evidence payload.
5. Focused tests; then `just sandboxed-check`; record host-only gates as NOT
   RUN.

## 5. Acceptance

- [ ] A fixture task reaching `completed` with malformed final JSON still reads
      back as `completed` and exposes the decode failure class.
- [ ] `invalid_json`, `invalid_schema`, `oversized`/`truncated`, and `missing`
      are distinguishable to the caller.
- [ ] Bounded raw final output is retrievable via `task_get`/evidence without
      re-running the task.
- [ ] `schema-invalid` JSON exercises the same raw-result path as non-JSON.
- [ ] Valid reports keep the existing behavior and shape byte-for-byte.
- [ ] Records written before this change still load and show a report state.
- [ ] Bounded limits are still enforced (explicit `truncated`, no silent loss).

## 6. Validation commands

- `cargo test --bin temote-mcp --all-features --locked opencode` / `devin`
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `just sandboxed-check`
- If `task_get` output shape changes the gateway contract:
  `TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT=1 cargo test --bin temote-mcp --all-features --locked gateway`
  and `(cd gateway && npm test)`.
- Host/live re-check against a real backend session: NOT RUN here — record as a
  live-acceptance-matrix row if the public shape changes.

## 7. Delivery authorization

One feature branch + one PR to `main`. Keep the diff limited to the report
result-delivery layer; no unrelated cleanup.

## 8. Completion report

(to be filled by the implementing packet run)
