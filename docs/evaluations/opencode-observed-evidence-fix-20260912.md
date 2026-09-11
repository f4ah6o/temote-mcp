# OpenCode observed evidence trust-boundary fix

## Result

- status: fixed and verified locally (pre-push; finalization commit marks it landed)
- baseline HEAD: `af95984fa9aecd134c9ca4a4ce1fdb3232eab811`
- implementation commit: `1a7f4f0b0d297255d45b8afa5c2451aecf92de54`
- report commit: the commit containing this report (see `git log -5 --oneline`)
- final HEAD: the status-only finalization commit listed in `git log -5 --oneline`; use `git rev-parse HEAD` for the exact hash
- pushed: yes
- tracked worktree status: clean except the pre-existing untracked `.worktrees/` (untouched)

## Finding

Parent-facing `observed.model` / `observed.reasoning_effort` came from adapter-extracted OpenCode event `Evidence`, while the canonical report `observed_model` / `observed_effort` were copied from the delegated model's self-reported JSON. The same result could therefore expose two different "observed" values, and model-authored self-report could claim observations the adapter never made. `opencode_observed_model()` also treated top-level `providerID` + `modelID` differently from the `part` shape: it returned the bare model and dropped the observed provider.

## Trust-boundary change

- `normalize_opencode_report(value, options, evidence)` now builds the canonical report's observed fields exclusively from `Evidence`:
  - `observed_model` = `evidence.observed_model` (null when the adapter observed none)
  - `observed_effort` = `evidence.observed_reasoning_effort` (null for OpenCode today; no event field currently provides it)
- The raw report's `observed_model` / `observed_effort` are still required fields, type-checked, and length-bounded (`≤256` chars or null); their values are validated but discarded. The exact field set (`additionalProperties: false`), status enum, summary marker, array/scalar bounds, and the 4096-byte canonical budget are unchanged.
- `requested_model` / `requested_effort` continue to come from the caller's `Options`, never from model output.
- Parent `observed.*` (already from `Evidence`) and canonical report observed fields now share one source of truth, so they cannot disagree.

### Source of truth

| Field | Source |
| --- | --- |
| `requested_model` / `requested_effort` (report), `requested.model` (parent) | Temote `Options` (caller request) |
| `observed_model` / `observed_effort` (report), `observed.model` / `observed.reasoning_effort` (parent) | Adapter `Evidence` extracted from OpenCode events only; null otherwise |

## Event shape / provider+model normalization

- Inspected 93 stored `events.jsonl` artifacts from the live Codex/OpenCode runs (pinned OpenCode `1.18.30`). None contained `modelID`, `providerID`, or `model` at the top level or inside `part`; the observed OpenCode event fields are `type`, `timestamp`, `sessionID`, `part.{id,messageID,sessionID,type,text,time,reasoning,tokens,cost,tool,callID,state,snapshot}`. Live runs therefore legitimately produced `observed.model = null`, and the fix must not fabricate one.
- `opencode_observed_model()` now normalizes both shapes symmetrically via a shared `compose_opencode_model`:
  - top-level `modelID` (or `model`) + top-level `providerID` → `provider/model`
  - `part.modelID` (or `part.model` ) + `part.providerID` → `provider/model` (existing behavior preserved)
  - model-only events without a provider return the bare model; the provider is never guessed
- Each component is bounded by `MAX_EVIDENCE_STRING_BYTES` (256) and the composed value is rejected if it exceeds that bound, so arbitrary strings cannot widen the evidence.

## Files changed

- `src/delegation/opencode.rs` — normalization now takes `&Evidence`; observed fields sourced from evidence; raw observed shape validation retained; top-level provider/model composition and bounds; new tests and fake CLI fixtures.

No other production code changed. The Codex adapter, `TEMOTE_OPENCODE_BIN` resolution, child environment allowlist, sandbox/process lifecycle, artifact bounds, and persistent server/session behavior are untouched.

## Tests added/updated

Added:

- `opencode_normalization_uses_event_evidence_not_self_reported_observed` — raw self-report `fake/self-reported` + `high` is replaced by evidence model and null effort; requested values stay canonical.
- `opencode_normalization_nulls_observed_without_event_evidence` — no evidence → canonical observed null even when the model self-reports.
- `opencode_normalization_still_validates_raw_observed_fields` — oversized observed value, wrong type, and missing field still fail closed.
- `opencode_observed_model_composes_top_level_provider_and_model` — top-level `providerID`+`modelID` and `providerID`+`model` compose.
- `opencode_observed_model_preserves_existing_shapes` — `part` composition, model-only top-level/part values, and empty object.
- `opencode_observed_model_does_not_guess_providers_or_exceed_bounds` — overlong composed values rejected; out-of-bound provider ignored rather than guessed; oversized model rejected.
- `opencode_event_evidence_overrides_self_reported_observed_model` (fake CLI) — event `opencode-go/deepseek-v4-flash` wins over the report's `fake/self-reported`; parent and report agree; effort null.
- `opencode_self_reported_observed_model_is_dropped_without_evidence` (fake CLI) — no event evidence → parent and report observed null.

Updated:

- `opencode_observed_model_and_variant_are_recorded` — now also asserts `report.observed_model == parent observed.model`.
- All `normalize_opencode_report` unit-test call sites pass explicit (usually empty) `Evidence`, making the evidence input part of the contract under test.

## Verification

Commands run on the final code (repository root):

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo test --bin temote-mcp opencode` | 71 passed / 0 failed |
| `cargo test --bin temote-mcp "cli::codex::delegation"` | 80 passed / 0 failed |
| `cargo test --bin temote-mcp codex` | 140 passed / 0 failed |
| `cargo test --all-targets --all-features --locked` | 632 bin + 40 lib + 9 e2e passed / 0 failed (1 ignored, pre-existing) |
| `(cd gateway && npm test)` | 60/60 passed |
| `git diff --check` | clean |

## Live smoke (installed OpenCode 1.18.30)

Read-only one-shot delegation from a disposable directory with `TEMOTE_OPENCODE_BIN` set to the installed binary:

- status: `success`, report `completed`, `artifacts_truncated=false`, exit code 0
- requested model: `opencode/mimo-v2.5-free` (both `requested.model` and `report.requested_model`); requested effort `""`
- parent `observed.model`: `null`; parent `observed.reasoning_effort`: `null`
- canonical `report.observed_model`: `null`; `report.observed_effort`: `null`
- worktree: disposable directory remained empty; no files created or changed

The live event stream contained no model/provider evidence, so null is the only correct observation; nothing was fabricated.

## Remaining limitations

- OpenCode 1.18.30 does not emit model/provider metadata in the inspected event stream, so live `observed_model` is normally null. The provider/model composition paths are covered by deterministic fixtures pending a CLI version that emits the fields.
- `observed_effort`/variant remains null because no OpenCode event field currently exposes it.
- Persistent OpenCode server/session/resume remains out of scope.

## Temote verification

session: `temo`
repository: `/Volumes/DevSSD/Developer/local-mcp`
Verify:
1. read this report
2. `git status --short`
3. `git rev-parse HEAD`
4. `git rev-parse origin/main`
5. `git show --stat --oneline HEAD`
6. spot-check `normalize_opencode_report` / `opencode_observed_model` in `src/delegation/opencode.rs` and the observed evidence tests
