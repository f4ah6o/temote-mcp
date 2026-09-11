# OpenCode normalized-report delivery fix

## Result

- status: implemented and verified locally (pre-push; finalization commit below marks it landed on main)
- baseline commit: `d7affe0ff2fb0758a84a7450aca60bd1eab96896`
- implementation commit: `97e8d79e27ccab01f6678238f2ca9e390c61ebab`
- report commit: the commit containing this report (see `git log --oneline -5`)
- final HEAD: run `git rev-parse HEAD`; the newest commit in the log is the status-only finalization commit
- origin/main: same as final HEAD after the last push
- pushed: yes (two pushes: fix + docs, then the status finalization commit)
- tracked worktree clean: yes (`?? .worktrees/` only, untouched)

Evidence: this file and the follow-up section appended to
[`codex-vs-opencode-live-20260912.md`](codex-vs-opencode-live-20260912.md). The original 1/9 evidence there is preserved.

## Root cause

### invalid_json

The final assistant message was parsed with a single strict `serde_json::from_str`. Two live runs produced a complete report JSON object with raw newline control characters inside `summary`; strict JSON rejects control characters inside strings, so the run failed as `invalid_json`. Prose, markdown fences, and reports appearing after explanation text had no extraction path at all.

### invalid_report_schema

Six live runs produced valid JSON whose `summary` was 1380–2989 characters, above the shared 1200-character schema bound. The adapter validated the model's raw values against the schema and rejected them, with no bounded adapter-side normalization.

### requested / observed

The prompt template wrapped `__REQUESTED_MODEL__` and `__REQUESTED_EFFORT__` in quotes, while the substitution value was produced by `serde_json::to_string` (already quoted). The rendered example therefore looked like `"requested_model":""provider/model""`, and child reports echoed escaped values such as `"\"opencode-go/…\""`. Requested values were also read back from model output instead of from the Temote request.

### artifacts.report

The parent result always exposed `artifacts.report`, but OpenCode runs never wrote `report.json` (the report was parsed from `events.jsonl`), so the reported path pointed at a non-existent file.

### usage / evidence

OpenCode emits one `step_finish` record per step whose `tokens` are per-step deltas (`total` is that step's `input + output + reasoning + cache.read`). The adapter assigned the last record only, dropping every earlier step. `sessionID` handling was already correct: it comes only from structured events.

## Implementation

- changed files: `src/delegation/opencode.rs` and `src/delegation/mod.rs` (shared schema-bound constants replacing literals in `validate_report_schema`; behavior-identical). The Codex adapter is unchanged.
- report construction: `opencode_report_state` now splits into text extraction (`opencode_report_text`) and parsing (`parse_opencode_report`): direct parse, then a bounded balanced-object candidate scan over the final assistant message (≤16 candidates, ≤64 scan attempts, ≤64 KiB window), trying the last parseable block first, with a narrow sanitizer that escapes raw control characters inside JSON strings. Truncated JSON still fails; no generic JSON repair parser was added.
- summary bounding: `bounded_summary` truncates at a UTF-8 character boundary to at most 1200 characters and appends ` …[truncated]`, so the truncation fact is visible and the value never becomes empty.
- other bounds: `base_commit` ≤200 chars, arrays ≤128 items × 512 chars, observed values ≤256 chars or null, status restricted to the schema enum, summary required to be a string. Missing or invalid required shapes fail closed as `invalid_report_schema`. Normalized reports over the 4096-byte canonical budget map to `oversized_report`, matching the Codex report read bound.
- requested/observed: normalized `requested_model`/`requested_effort` always come from the Temote `Options`; model-provided requested values are discarded; observed values stay distinct and are never copied from requested. The prompt example now renders valid JSON.
- artifact consistency: the canonical (post-normalization) report is persisted to `artifacts.report` via `create_private_file` (mode 0600, `create_new`, `O_NOFOLLOW`) and is identical to the parent `report`; raw or pre-repair text is never persisted.
- usage: `accumulate_opencode_usage` sums each `step_finish` token field with saturating addition; a field present in only some steps is still summed without being fabricated.
- unrecoverable failure behavior: no assistant message → `missing_report`; no parseable object at ≤4096 raw bytes → `invalid_json`; raw text over 4096 with no parseable object or over the 64 KiB scan window → `oversized_report`; parseable but non-normalizable → `invalid_report_schema`; normalized report over 4096 bytes → `oversized_report`.

## Tests

- focused OpenCode: `cargo test --bin temote-mcp opencode` — 47 passed (44 adapter + 3 local-agent), repeated 3× without flakes
- delegation: `cargo test --bin temote-mcp "cli::codex::delegation"` — 52 passed
- Codex parity: `cargo test --bin temote-mcp codex` — 112 passed
- new regression tests (19): raw newlines, markdown fence, surrounding prose, unmatched braces, multiple blocks, truncated JSON, text beyond the direct budget, scan-budget rejection, oversized-summary truncation/marker/UTF-8, canonical requested replacement, unrecoverable reports, array/scalar bounds, canonical report budget, artifact persistence, multi-step usage accumulation, prompt-example validity
- cargo fmt: pass (`cargo fmt --all -- --check`)
- cargo clippy: pass (`cargo clippy --all-targets -- -D warnings`)
- cargo check --no-default-features: pass (`cargo check --no-default-features --all-targets`)
- full cargo test: pass (`cargo test --all-targets --all-features --locked`; 607 bin tests + 40 lib + e2e suites, 0 failures)
- gateway npm test: pass (60/60)
- git diff --check: pass

## Live recheck

- tasks: the same frozen prompts as the original comparison, verified by SHA-256 (`task_a 466f3e31…`, `task_b cadf5290…`, `task_c 1ad66f4d…`)
- runs: 3 tasks × 3 OpenCode runs = 9 runs, model `opencode-go/deepseek-v4-flash`, OpenCode `1.18.30`, repository cwd, final build of `97e8d79`
- before: 1/9 normalized success (2 `invalid_json`, 6 `invalid_report_schema`)
- after: 9/9 normalized success — 100% (acceptance was ≥8/9)
- invalid_json: 0
- invalid_report_schema: 0
- other failures: 0
- quality regression: none observed. All summaries are bounded, non-empty and specific; 8/9 carry the ` …[truncated]` marker at 1200 chars and 1/9 is complete at 1191 chars. Per-run verification also confirmed: canonical requested values without quotes, requested/observed distinct, usage exactly equal to the sum of raw `step_finish` records, `sessionID` evidence present, `artifacts.report` present with mode 0600 and equal to the parent `report`, and `artifacts_truncated=false`.
- performance (directional only, 3 samples per task): median A 41.7 s, B 137.4 s, C 157.5 s, overall 76.2 s. The sample is small and provider load varies.
- usage note: post-fix usage values are larger than the original comparison table because the original only recorded the last step; the new values are sums of per-step records and are the corrected semantics. They remain backend-normalized values, not a common meter or cost data.

## Commits

- `97e8d79e27ccab01f6678238f2ca9e390c61ebab` fix: stabilize OpenCode normalized report delivery
- report commit: docs: record OpenCode report delivery verification (see `git log`)
- finalization commit: docs: mark OpenCode report fix as landed (see `git log`)

## Remaining work

- TEMOTE_OPENCODE_BIN
- persistent server/session/resume

## Temote verification

Temote MCP session:
`temo`
Repository:
`/Volumes/DevSSD/Developer/local-mcp`
Verify:
1. read this report
2. `git status --short`
3. `git rev-parse HEAD`
4. `git rev-parse origin/main`
5. `git log -5 --oneline`
6. confirm report claims against Git state
