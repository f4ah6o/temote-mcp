# S1: single backend-neutral source for the task report schema

Status: done
Model: unknown
Created: 2026-09-27
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Create one report-contract source while retaining each backend’s existing compatibility validation profile.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Create one report-contract source while retaining each backend’s existing compatibility validation profile.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

Keep the existing backend-specific compatibility profiles and acceptance differences while centralizing shared field definitions and bounds. Do not replace them with one stricter validator or claim wire-identical profiles.

### Preserved fixed contract: 2. Fixed decisions

- One canonical module (new `src/report_contract.rs`) owns:
  - the field catalog (currently `REPORT_FIELDS` at `src/delegation/mod.rs:25`),
  - the bound constants (`MAX_REPORT_*` at `src/delegation/mod.rs:37-41`;
    `MAX_SUMMARY_CHARS`/`MAX_REPORT_ARRAY_ITEMS`/`MAX_REPORT_BYTES` at
    `src/opencode_server.rs:43-45`, `src/devin_acp.rs:42-43`,
    `src/devin_cloud.rs:41-43`),
  - a profile-parameterized validator,
  - a `report_json_schema()` producer replacing `devin_cloud::report_schema()`
    (`src/devin_cloud.rs:85`).
- Two named compatibility profiles, chosen explicitly at each callsite:
  - `ReportProfile::Delegation` — today's strict evidence-file contract:
    exactly the 10 `REPORT_FIELDS`, all required, `status` enum, bounded
    strings/arrays, `observed_*` nullable (currently
    `delegation::validate_report_schema`, `src/delegation/mod.rs:1021`, consumed
    by `read_report` at line 981 → `ReportState` at line 215).
  - `ReportProfile::TaskReport` — today's lenient task-report contract shared
    identically by `opencode_server::report_shape_valid`
    (`src/opencode_server.rs:3377`), `devin_acp`'s copy
    (`src/devin_acp.rs:2774`), and `devin_cloud`'s copy
    (`src/devin_cloud.rs:1483`): required `status` (enum) + `summary`
    (≤ `MAX_SUMMARY_CHARS`); optional `base_commit` and
    `changed_files`/`checks`/`unresolved` bounded string arrays; serialized
    byte bound.
- These profiles are *intentionally different* — the Delegation profile
  requires requested/observed model+effort fields the TaskReport profile does
  not define, and Devin Cloud's wire schema cannot even carry them
  (`additionalProperties: false`, required `["status","summary"]` at
  `src/devin_cloud.rs:95-97`). This packet pins the divergence; unifying the
  contracts is a separate change with its own migration decision (recorded as a
  parent-issue question for S2+).
- `report_json_schema()` output for Devin Cloud is byte-identical to today's
  `report_schema()` output — pinned by a snapshot test.
- Bound constants that differ per backend stay parameterized inputs to the
  validator (or per-backend overrides), never silently unified; remaining
  divergences are documented in the parent issue.
- `src/codex_app_server.rs` has **no** report validation path today (no
  `extract_report`/`report_shape_valid` — Codex tasks do not produce the shared
  report object). Nothing to migrate there; record this finding in the parent
  issue as input to S2.
- No backend gains or loses a field; public `report` object shape and the
  `ReportState` classification are unchanged.

## 受け入れ条件

Complete source criteria from “5. Acceptance” (unchecked items remain unverified):

- [x] Exactly one module defines the field catalog, bound constants, profiles, and the JSON-Schema producer.
- [x] Devin Cloud emits a `structured_output_schema` identical to the pinned snapshot (no wire change).
- [x] Each backend's validator accepts and rejects exactly the fixture set it accepts/rejects on `main` — per-profile characterization tests pin this; the Delegation↔TaskReport divergence is asserted explicitly, not unified.
- [x] Bound differences between backends, if any remain, are explicit named parameters documented in the parent issue — not accidental drift.
- [x] The codex_app_server no-validation finding is recorded in the parent issue for S2.
- [x] `cargo test --locked`, clippy, fmt, no-default-features check, and `git diff --check` are clean.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 6. Validation commands

- `cargo test --bin temote --all-features --locked report`
- `cargo test --bin temote --all-features --locked delegation`
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `(cd fabric && npm test)` — only if the wire surface changed; this packet
  should produce a zero-diff contract snapshot.

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 検証記録

- 2026-10-06: shared field catalog, strict Delegation and lenient TaskReport profiles, pinned Cloud schema and per-backend characterization PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/report_contract.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.

## 既存設計・履歴

> Historical Status: ready (revised 2026-09-27 per PR #73 review: profile split; the
single-validator/identical-fixture design was dropped because it conflicts
with no-behavior-change).
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `c305e41`
Parent issue: `issues/open/20260927-native-structured-output-agent-backends.md`
Prerequisites: none (refactor-only slice; no wire or validation-behavior change)

## 1. Goal

The Temote task report contract (field catalog, bound constants, validators,
and the JSON Schema sent to backends that support native structured output) is
produced from exactly one backend-neutral module. Per-backend copies stop
drifting — but each backend keeps its current acceptance contract, because the
contracts are *not* identical today (see Fixed decisions).

## 2. Fixed decisions

- One canonical module (new `src/report_contract.rs`) owns:
  - the field catalog (currently `REPORT_FIELDS` at `src/delegation/mod.rs:25`),
  - the bound constants (`MAX_REPORT_*` at `src/delegation/mod.rs:37-41`;
    `MAX_SUMMARY_CHARS`/`MAX_REPORT_ARRAY_ITEMS`/`MAX_REPORT_BYTES` at
    `src/opencode_server.rs:43-45`, `src/devin_acp.rs:42-43`,
    `src/devin_cloud.rs:41-43`),
  - a profile-parameterized validator,
  - a `report_json_schema()` producer replacing `devin_cloud::report_schema()`
    (`src/devin_cloud.rs:85`).
- Two named compatibility profiles, chosen explicitly at each callsite:
  - `ReportProfile::Delegation` — today's strict evidence-file contract:
    exactly the 10 `REPORT_FIELDS`, all required, `status` enum, bounded
    strings/arrays, `observed_*` nullable (currently
    `delegation::validate_report_schema`, `src/delegation/mod.rs:1021`, consumed
    by `read_report` at line 981 → `ReportState` at line 215).
  - `ReportProfile::TaskReport` — today's lenient task-report contract shared
    identically by `opencode_server::report_shape_valid`
    (`src/opencode_server.rs:3377`), `devin_acp`'s copy
    (`src/devin_acp.rs:2774`), and `devin_cloud`'s copy
    (`src/devin_cloud.rs:1483`): required `status` (enum) + `summary`
    (≤ `MAX_SUMMARY_CHARS`); optional `base_commit` and
    `changed_files`/`checks`/`unresolved` bounded string arrays; serialized
    byte bound.
- These profiles are *intentionally different* — the Delegation profile
  requires requested/observed model+effort fields the TaskReport profile does
  not define, and Devin Cloud's wire schema cannot even carry them
  (`additionalProperties: false`, required `["status","summary"]` at
  `src/devin_cloud.rs:95-97`). This packet pins the divergence; unifying the
  contracts is a separate change with its own migration decision (recorded as a
  parent-issue question for S2+).
- `report_json_schema()` output for Devin Cloud is byte-identical to today's
  `report_schema()` output — pinned by a snapshot test.
- Bound constants that differ per backend stay parameterized inputs to the
  validator (or per-backend overrides), never silently unified; remaining
  divergences are documented in the parent issue.
- `src/codex_app_server.rs` has **no** report validation path today (no
  `extract_report`/`report_shape_valid` — Codex tasks do not produce the shared
  report object). Nothing to migrate there; record this finding in the parent
  issue as input to S2.
- No backend gains or loses a field; public `report` object shape and the
  `ReportState` classification are unchanged.

## 3. Read / change scope

- `src/report_contract.rs` (new): field catalog, bound constants, profiles,
  validator, `report_json_schema()`.
- `src/delegation/mod.rs`: `REPORT_FIELDS` (25), `MAX_REPORT_*` (37-41),
  `validate_report_schema` (1021), `ReportState` (215), `read_report` (981) —
  delegate to `ReportProfile::Delegation`.
- `src/opencode_server.rs`: `report_shape_valid` (3377) and its callsite in
  `extract_report` (3327/3333), bound consts (43-45); `REPORT_INSTRUCTIONS`
  (line 112) stays as prompt text.
- `src/devin_acp.rs`: `extract_report`/`report_shape_valid` (2724/2774), bound
  consts (42-43).
- `src/devin_cloud.rs`: `report_schema()` (85), `report_shape_valid` (1483),
  `structured_output_schema` callsite (~2246), bound consts (41-43).
- `src/main.rs`: register the new module.
- `src/codex_app_server.rs`: read-only — confirm the no-validation finding.

## 4. Steps

1. Diff the validator/schema implementations; write the divergence table
   (fields required, optional fields, bounds, byte caps) into the parent issue
   before writing code.
2. Create `src/report_contract.rs` with the field catalog, constants, the two
   profiles, the parameterized validator, and `report_json_schema()`;
   re-export only what callers need.
3. Migrate each callsite to its profile; add the snapshot test pinning Devin
   Cloud's serialized JSON Schema byte-for-byte.
4. Add per-profile characterization tests: fixtures pinned against each
   backend's *current* accept/reject outcome (e.g. a report missing
   `requested_model` must still be accepted under TaskReport and rejected under
   Delegation — that divergence is the pinned contract, not a bug).
5. Focused tests; `just sandboxed-check`; host gates recorded as NOT RUN.

## 5. Acceptance

- [ ] Exactly one module defines the field catalog, bound constants, profiles,
      and the JSON-Schema producer.
- [ ] Devin Cloud emits a `structured_output_schema` identical to the pinned
      snapshot (no wire change).
- [ ] Each backend's validator accepts and rejects exactly the fixture set it
      accepts/rejects on `main` — per-profile characterization tests pin this;
      the Delegation↔TaskReport divergence is asserted explicitly, not
      unified.
- [ ] Bound differences between backends, if any remain, are explicit named
      parameters documented in the parent issue — not accidental drift.
- [ ] The codex_app_server no-validation finding is recorded in the parent
      issue for S2.
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

One feature branch + one PR to `main`. Refactor only; per-backend behavior
parity is the acceptance bar — do NOT unify the two profiles in this packet.

## 8. Completion report

(to be filled by the implementing packet run)
