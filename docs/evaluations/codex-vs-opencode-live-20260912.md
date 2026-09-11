# Codex vs OpenCode live comparison (2026-09-12, Asia/Tokyo)

Status: live comparison landed on main on 2026-09-12; measurement/evidence only, with no production code changes.

## Baseline and environment

- Baseline commit: `0f8a795d21ef45f20d9b317cfc16ffb0c71cbcba` (`main`).
- Binary under measurement: debug build from that commit (`cargo build`).
- Host: macOS `26.5.2`, `arm64`.
- Codex CLI: `codex-cli 0.153.4`, installed at the operator's Vite+ (`vp`) tool path; model `gpt-5.6-luna`, `--reasoning-effort high`.
- OpenCode CLI: `1.18.30`; `delegate diagnose --backend opencode` reported `models=ready count=64` and `opencode-go/deepseek-v4-flash` present before the runs.
- Working directory for every run: the repository root. All three tasks are read-only and instruct the worker not to modify files. `git status --short` was checked between batches; only the pre-existing untracked `.worktrees/` appeared and no tracked file changed.
- The three prompts were frozen before the first run; their SHA-256 hashes are recorded below.
- Raw per-run JSON, stderr, and metadata were kept outside the repository for analysis; only non-secret aggregates are recorded here.

## Tasks (verbatim, `sha256`)

Task A — repository comprehension:

```text
Read the delegation backend implementation under src/delegation/ and explain:
1. how Codex and OpenCode are selected,
2. which security boundaries are shared by both backends,
3. which logic is backend-specific,
4. the three most important remaining risks.
Do not modify any files. Return concise structured text only.
```

Task B — targeted review:

```text
Review src/delegation/opencode.rs for correctness and maintainability.
Identify at most five concrete issues or risks.
For each, give file/function context and explain why it matters.
Do not modify files.
If no meaningful issue is found, say so.
```

Task C — implementation planning:

```text
Read issues/open/20260910-opencode-delegation-backend.md and the current delegation adapters under src/delegation/.
Propose the smallest safe next slice after the currently landed work.
Do not implement it.
Include scope, non-goals, tests, and acceptance criteria.
```

| Task | SHA-256 |
| --- | --- |
| A | `466f3e3132ddc9d44d427ca81fb14c927c0f88fc8d9e1ea0b1d58c9496fbb109` |
| B | `cadf529085351f9b781524944a41fa6f4c3ea2dd58522e567e7cfc93d4694aa2` |
| C | `1ad66f4d4b2d326121867dca167fbd1bea37ba077eecf468c215b849d8b69167` |

## Execution

- Matrix: 3 tasks × 2 backends × 3 runs = 18 delegation runs, executed sequentially between `2026-09-11T16:22:57Z` and `2026-09-11T17:10:19Z` (JST: 2026-09-12 01:22–02:10).
- Commands were the existing CLI surfaces only:
  - `temote-mcp delegate --backend codex --model gpt-5.6-luna --reasoning-effort high --prompt-file <task>`
  - `temote-mcp delegate --backend opencode --model opencode-go/deepseek-v4-flash --prompt-file <task>`
- No backend-specific prompt wrapper or cwd was added by the measurement harness.
- Harness note: the first 3 OpenCode attempts never reached the backend. The local runner failed on an empty optional-argument array under `set -u` (bash 3.2) and exited in ~20 ms. These attempts are recorded separately as harness failures and are excluded from backend metrics; the runner was fixed and the 9 formal OpenCode runs were then executed.

## Results (per run)

| Backend | Task | Run | Normalized status | Duration (ms) | Input | Cached input | Output | Reasoning | Total | Report / raw length |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| codex | A | 1 | success | 132269 | 506448 | 423424 | 6521 | 4452 | — | 2402 B report |
| codex | A | 2 | success | 135759 | 489483 | 404224 | 6241 | 4215 | — | 2038 B report |
| codex | A | 3 | success | 135867 | 439353 | 352256 | 6467 | 3748 | — | 2335 B report |
| codex | B | 1 | success | 363277 | 3604440 | 3402752 | 15732 | 11826 | — | 1628 B report |
| codex | B | 2 | success | 180073 | 841371 | 736256 | 8895 | 5675 | — | 1587 B report |
| codex | B | 3 | success | 446060 | 2574286 | 2429952 | 21049 | 16105 | — | 1733 B report |
| codex | C | 1 | success | 162407 | 740671 | 632576 | 7469 | 4538 | — | 1593 B report |
| codex | C | 2 | success | 184267 | 938232 | 829184 | 8710 | 4701 | — | 1646 B report |
| codex | C | 3 | success | 176325 | 772605 | 682752 | 8541 | 5569 | — | 1605 B report |
| opencode | A | 1 | invalid_json | 44662 | 320 | 47104 | 915 | 0 | 48339 | 3657 B raw |
| opencode | A | 2 | invalid_json | 115949 | 1613 | 47360 | 5707 | 0 | 54680 | 1973 B raw |
| opencode | A | 3 | success | 118022 | 97 | 55296 | 445 | 0 | 55838 | 1480 B report |
| opencode | B | 1 | invalid_report_schema | 87294 | 285 | 51712 | 3076 | 0 | 55073 | 2447 B raw |
| opencode | B | 2 | invalid_report_schema | 153387 | 154 | 54016 | 688 | 0 | 54858 | 2999 B raw |
| opencode | B | 3 | invalid_report_schema | 153822 | 58 | 50176 | 813 | 0 | 51047 | 3111 B raw |
| opencode | C | 1 | invalid_report_schema | 80574 | 323 | 68352 | 751 | 0 | 69426 | 3252 B raw |
| opencode | C | 2 | invalid_report_schema | 42223 | 128 | 59136 | 668 | 0 | 59932 | 2375 B raw |
| opencode | C | 3 | invalid_report_schema | 54880 | 256 | 70819 | 2211 | 0 | 70819 | 3596 B raw |

Session/thread evidence was present for every run that reached a backend. `artifacts_truncated` was false in all 18 runs. The two `invalid_json` OpenCode runs produced a complete final assistant message but with raw control characters (newlines) inside JSON strings; the six `invalid_report_schema` runs produced valid JSON whose `summary` exceeded the shared 1200-character bound (1380–2989 chars).

## Performance

Median wall-clock duration per backend and task (3 runs each):

| Task | Codex median | OpenCode median |
| --- | ---: | ---: |
| A (comprehension) | 135.8 s | 115.9 s |
| B (review) | 363.3 s | 153.4 s |
| C (planning) | 176.3 s | 54.9 s |
| All | 176.3 s | 87.3 s |

Notable variance: Codex Task B ranged 180–446 s (≈2.5×) while OpenCode Task A ranged 44.7–118.0 s. Single fast runs were not treated as representative; medians are over only three samples per cell. Provider congestion and token counts differ per run, so latency differences are directional, not definitive.

## Quality comparison

Procedure: for each task, the six answers were shuffled into labels `A`–`F` with a fixed seed and evaluated without backend labels; the mapping was restored afterwards. Each answer received one ordinal score on a 0–3 scale (0 unusable, 1 weak, 2 acceptable, 3 strong) reflecting correctness, relevance, specificity, and hallucination risk together.

Delivered results: Codex 9/9 normalized `success`; OpenCode 1/9 (`invalid_json` ×2, `invalid_report_schema` ×6). The blind content scores below judge the model's underlying answer, not deliverability.

| Backend | Task A | Task B | Task C | Content average |
| --- | --- | --- | --- | ---: |
| Codex | 3, 3, 3 | 2, 2, 2 | 3, 3, 3 | 2.67 |
| OpenCode (raw where undelivered) | 3, 3, 2 | 2, 3, 2 | 3, 3, 3 | 2.67 |

Observations:

- Content quality was close in this sample. Both backends correctly described backend selection, shared boundaries, backend-specific logic, and the main risks.
- Codex answers were constrained by the report contract: every Codex `summary` was exactly the 1200-character maximum and was cut mid-sentence, which dropped the last findings of Task B (the model placed no fallback into `unresolved` there). The arrays (`checks`, `unresolved`) carried some risk items but not all.
- OpenCode raw answers were longer and often more specific (e.g., `opencode_effective_prompt` double-quoting, version-truncation classification, and the non-existent `artifacts.report` path). Because 8/9 were not normalized, they were not usable as delegation results without manual inspection of raw artifacts.
- Hallucinations or factual errors observed: one OpenCode Task A answer claimed OpenCode requires `--variant` (it is optional); one OpenCode Task B answer claimed oversized prompts fail only at run time rather than validation (current `opencode::validate_options` calls `opencode_effective_prompt`, so the 64 KiB bound fails during validation); one Codex Task B answer reported "full cargo test" results that in fact match only the 40 lib tests. No invented functions, files, or issue states were observed beyond those errors.
- Best individual review answer in the blind pass found all five real issues in `opencode.rs` (timeout descendant cleanup, version-truncation classification, model-listing parser fragility, diagnostics duplication/labeling, and the non-existent `report.json` artifact path).

## Usage

Median normalized usage per task:

| Task | Codex input / cached / output / reasoning | OpenCode input / cached / output / total |
| --- | --- | --- |
| A | 489,483 / 404,224 / 6,467 / 4,215 | 320 / 47,360 / 915 / 54,680 |
| B | 2,574,286 / 2,429,952 / 15,732 / 11,826 | 154 / 51,712 / 813 / 54,858 |
| C | 772,605 / 682,752 / 8,541 / 4,701 | 256 / 68,352 / 751 / 69,426 |

Comparability caveat: these are the backends' own normalized usage fields, not a common meter. Codex reports cumulative turn usage (including retries and cache reads) and does not emit a total; OpenCode's `input` excludes cache reads while its `total` includes them, and its values reflect prompt-only wrapper overhead. Direct token-for-token or cost comparison is not supported by this data, and no price estimates are made.

## Observed issues (recorded, not fixed)

1. **OpenCode normalized-report delivery fails for non-trivial answers (8/9 runs).** Reproduction: run Task B with `delegate --backend opencode --model opencode-go/deepseek-v4-flash`. Impact: review/planning quality is not reachable through the delegation contract; callers receive `invalid_json` (raw newlines in strings) or `invalid_report_schema` (summary 1380–2989 chars against the 1200-char bound). Suggested follow-up: enforce or repair the final report (strict structured result, validation with bounded retry, or a split summary field) rather than prompt-only compliance.
2. **The embedded report example double-quotes requested values.** `opencode_effective_prompt` substitutes JSON-quoted values into placeholders already surrounded by quotes, producing `"requested_model":""provider/model""`; child reports echoed `"\"opencode-go/…\""`. Reproduction: any successful OpenCode run. Suggested follow-up: fix the template/replacement contract.
3. **Pre-launch failures leak artifact directories.** `run_opencode` creates artifacts before prompt/cwd resolution and spawn; those early error paths do not remove the directory, unlike the diagnostics probe. Suggested follow-up: shared cleanup guard.
4. **The timeout kills only the direct child.** A descendant holding stdout/stderr can keep capture threads blocked past the deadline. Reproduction: a fake child that forks a sleeper (the original test fixture exhibited this until it was changed to `exec`). Suggested follow-up: process-group or job-tree termination plus bounded pipe draining.
5. **`artifacts.report` is misleading for OpenCode.** The path is reported in the parent result but `report.json` is never written for OpenCode (the report is parsed from `events.jsonl`). Suggested follow-up: omit or relabel the field per backend.
6. **Evidence semantics need documentation.** `observed_model` is first-event wins and `usage` is last `step_finish` only, so multi-step OpenCode sessions can misreport model and undercount usage. The Codex backend has no wall-clock timeout at all. These are documented behaviors but were independently flagged by both backends during the review task.

## Limitations

- Three runs per backend per task, one model per backend, one host, one day: results are directional only.
- Codex content was constrained by the 1200-character summary bound; OpenCode raw content was not, so content scores are not perfectly symmetric. Delivery status is the decisive practical difference in this sample.
- The evaluator is the repository maintainer and knew the implementation while scoring; the blind labeling reduced but did not eliminate bias, and OpenCode answers were longer because they were raw.
- Usage units differ by backend and are not cost data. No pricing was inferred.

## Recommendation

- Keep Codex as the default delegation backend. In this sample it was the only backend that reliably delivered normalized, schema-valid results across comprehension, review, and planning tasks.
- Use Codex for bounded structured delegation results and for review/planning where the strict report contract and deterministic status matter.
- Use OpenCode for interactive or session-based work today, or as a delegation backend after the report-delivery gap is fixed; its raw analysis was competitive and its median latency was lower, but unnormalized output makes it a poor default for one-shot delegated results. As a fallback it should come with explicit delivery validation.
- Not enough evidence yet for latency or cost conclusions, or for claiming general model superiority. A follow-up with more runs, a second OpenCode model, and the report-contract fix would be needed.

## Follow-up: OpenCode normalized-report delivery fix (2026-09-12)

Status: fix implemented and verified locally; original 1/9 evidence above is preserved unchanged.

### Before

At baseline `d7affe0` (the comparison above), OpenCode delivered normalized results in 1/9 runs:

- `invalid_json` ×2: the final assistant message was a complete JSON object containing raw newline control characters inside string values, which strict `serde_json` parsing rejected.
- `invalid_report_schema` ×6: the report parsed but the model put 1380–2989 characters into `summary`, above the shared 1200-character schema bound.

Additional defects observed: the report-contract example double-quoted requested values (`"requested_model":""provider/model""`), `artifacts.report` named a file that OpenCode runs never wrote, and multi-step `step_finish` usage was overwritten per step (last step only) instead of accumulated.

### Root cause

- Delivery depended entirely on the model emitting strict JSON within all schema bounds, with no adapter-side repair or normalization.
- The prompt template wrapped `__REQUESTED_MODEL__`/`__REQUESTED_EFFORT__` placeholders in quotes while the substitution value was already JSON-quoted.
- OpenCode `step_finish` token records are per-step; the adapter assigned instead of summing.
- No canonical report artifact was persisted for OpenCode.

### Implemented fix

Changed files: `src/delegation/opencode.rs` (adapter) and `src/delegation/mod.rs` (shared schema-bound constants replacing literals in `validate_report_schema`; behavior-identical for Codex). Codex adapter untouched.

- Report extraction: direct parse first, then a bounded balanced-object candidate scan (≤16 candidates, ≤64 scan attempts, ≤64 KiB scan window) over the final assistant message, trying the last parseable block first with a narrow raw-control-character sanitizer inside strings. No generic JSON repair parser.
- Deterministic normalization: `requested_model`/`requested_effort` are replaced with the canonical Temote request values; `summary` is truncated at a UTF-8 character boundary to ≤1200 characters with a ` …[truncated]` marker; `base_commit` ≤200; arrays ≤128 items × 512 characters; observed values ≤256 or null; missing/invalid status, missing summary, or non-string fields fail closed as `invalid_report_schema`.
- Canonical report budget: normalized reports over 4096 bytes map to `oversized_report`, matching the Codex report read bound.
- One normalization path: the canonical report is persisted to `artifacts.report` (mode 0600, `create_new`, `O_NOFOLLOW`) and is byte-equivalent to the parent result `report`; only post-normalization content is written.
- Usage: per-step `step_finish` token fields are accumulated with saturating addition; no double count (verified equal to raw event sums) and no drop of earlier steps.
- Prompt contract example now renders valid JSON (placeholders unquoted before JSON-quoted substitution).

### Regression tests

Added 19 deterministic regression tests (44 OpenCode adapter tests total): raw newlines, markdown fences, surrounding prose, unmatched braces, multiple blocks, truncated JSON, text beyond the direct budget, scan-budget rejection, oversized-summary normalization with UTF-8 boundary and marker, canonical requested replacement, unrecoverable reports, array/scalar bounds, canonical report budget, canonical artifact persistence, accumulated multi-step usage, and prompt-example validity. All focused suites were run repeatedly without flakes.

### After (post-fix live recheck, final build)

Same frozen prompts (hashes unchanged) and model, 3 tasks × 3 runs:

| Task | Run | Status | Duration | Summary chars | Truncated | Steps | Input | Cached | Output | Total |
| --- | --- | --- | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| A | 1 | success | 69928 | 1200 | yes | 9 | 67331 | 348160 | 4187 | 419678 |
| A | 2 | success | 25602 | 1200 | yes | 4 | 45785 | 73728 | 2129 | 121642 |
| A | 3 | success | 41703 | 1200 | yes | 7 | 38038 | 187904 | 4304 | 230246 |
| B | 1 | success | 76221 | 1200 | yes | 5 | 38285 | 125440 | 8258 | 171983 |
| B | 2 | success | 199372 | 1200 | yes | 5 | 57484 | 153856 | 12259 | 223599 |
| B | 3 | success | 137384 | 1200 | yes | 5 | 40328 | 146176 | 10599 | 197103 |
| C | 1 | success | 166840 | 1200 | yes | 9 | 58931 | 400640 | 6793 | 466364 |
| C | 2 | success | 157512 | 1191 | no | 19 | 68041 | 1190400 | 11210 | 1269651 |
| C | 3 | success | 53998 | 1200 | yes | 10 | 75542 | 484608 | 5039 | 565189 |

- Success rate: before 1/9 (11%); after 9/9 (100%). `invalid_json`: 2 → 0. `invalid_report_schema`: 6 → 0. Other failures: 0 → 0.
- Median wall-clock: A 41.7 s, B 137.4 s, C 157.5 s; overall 76.2 s (before: 115.9/153.4/54.9, overall 87.3 s). The sample is small and runs vary with provider load; durations are directional only.
- Every run verified: canonical `requested_model` without quotes, requested/observed distinct, non-empty bounded summary with an accurate truncation marker, usage equal to the sum of raw `step_finish` records, `sessionID` evidence present, `artifacts.report` present with mode 0600 and identical to the parent `report`, `artifacts_truncated=false`.
- Usage numbers are larger than the before table because before reported only the last step; the after values are per-step sums and are the corrected semantics. They are still provider-specific normalized values, not a common meter or cost data.

### Remaining limitations

- The 1200-character `summary` bound means long review/planning answers are truncated (8/9 runs ended with the marker). The delivered prefix stays specific and readable, but the model rarely uses the `checks`/`unresolved` arrays for detail. A future schema/structure change is out of scope for this fix.
- `observed_model` remains null in these runs because the OpenCode event stream did not expose model metadata; it is not copied from `requested_model`.
- Extraction is deliberately bounded (64 KiB scan, 16 candidates, 64 attempts); exotic output beyond that maps to deterministic failure statuses rather than being repaired.
