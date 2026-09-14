# OpenCode delegation review fixes

## Result

- status: fixed and landed on main
- baseline HEAD: `3f41b2108ebba6acb475c3038f412e401ef54a05`
- implementation commit: `3ba57681d56a431c970eda5f7c93cca0be8fbcb2`
- report commit: `ec50340b61d327b09244e7a5f8d9d9b62023c1f0`
- final HEAD: the status-only finalization commit listed in `git log -5 --oneline`; use `git rev-parse HEAD` for the exact hash
- pushed: yes
- tracked worktree clean: yes (`?? .worktrees/` only, untouched)

## Files changed

- `src/delegation/opencode.rs` — strict normalization, extra-field rejection, artifact cleanup guard, updated/new tests
- `src/delegation/mod.rs` — promoted the report field list to a shared constant used by the schema validator and the OpenCode normalizer (behavior-preserving for Codex)

No other production code changed. The Codex adapter, executable override behavior, child environment allowlist, and parent result bounds are untouched.

## Review findings and resolutions

### 1. Silent truncation in normalized reports

Finding: `bounded_report_items` truncated `changed_files`/`checks`/`unresolved` to 128 items and each item to 512 chars, and `bounded_report_text` truncated `base_commit`/observed strings, while the run still returned `success`. Oversized `unresolved[128..]` could disappear silently.

Resolution: normalization now fails closed instead of silently truncating:

- arrays with more than 128 items → `invalid_report_schema`
- any array item longer than 512 chars → `invalid_report_schema`
- `base_commit` longer than 200 chars → `invalid_report_schema`
- `observed_model`/`observed_effort` longer than 256 chars → `invalid_report_schema`

`summary` keeps its existing explicit truncation-evidence mechanism: values over 1200 chars are cut at a UTF-8 character boundary and end with the visible ` …[truncated]` marker. The canonical 4096-byte report budget is unchanged, and values that are within schema bounds but still exceed the canonical budget map to `oversized_report` (fail closed, no success). Parent result size limits are unchanged.

Tests added: `opencode_normalization_rejects_oversized_arrays_and_scalars`, `opencode_normalization_accepts_values_at_the_schema_bounds` (at-bound values normalize; oversized canonical result still fails the budget), and the end-to-end `opencode_oversized_array_is_not_silently_truncated` via a fake CLI mode producing 129 `unresolved` items (status `invalid_report_schema`, `report=null`).

### 2. Raw-newline regression test did not reproduce invalid JSON

Finding: the old test built a `serde_json::Value` and called `.to_string()`, which escapes newlines as `\n`, so the input was valid JSON and the test did not reproduce the live failure.

Resolution: `opencode_report_repairs_raw_newlines_in_strings` now constructs the report through `concat!` with a literal newline inside the `summary` string. The test first asserts `serde_json::from_str` rejects the fixture (proving it is genuinely invalid JSON), then asserts the bounded parser repairs it and the parsed `summary` contains the newline. The negative case `opencode_report_rejects_truncated_json` (unrecoverable/truncated JSON remains `invalid_json`) is retained. No permissive generic JSON repair was introduced.

### 3. Artifact cleanup on early OpenCode launch failures

Finding: `run_opencode` created artifacts before fallible steps (prompt wrapping, cwd canonicalization, process spawn, wait, status inspection, report persistence); these `?` error paths returned without removing the `temote-codex-delegation-*` directory.

Resolution: added a small RAII `ArtifactCleanup` guard. It removes the artifact directory on drop unless `keep()` was called. `keep()` is called only on result-bearing paths — timeout, non-zero exit, and the final result — so the parent still receives artifact paths for intentional success/failure evidence. Early error paths (including report persistence failure) drop the guard and clean up. There are no cleanup races: the guard owns the directory until a result is produced, and no path is returned after cleanup.

Tests added: `opencode_artifact_cleanup_removes_the_directory_unless_kept` (guard semantics) and an assertion in the early-launch `opencode_missing_executable_is_backend_unavailable` test that the cleanup actually ran (test-only atomic counter incremented by the guard's drop).

### 4. Extra-field strictness

Finding: `normalize_opencode_report` read known fields and rebuilt a canonical object, so a raw parsed report containing unexpected top-level fields could still be accepted even though the contract is `additionalProperties: false`.

Resolution: normalization rejects any raw object whose key count is not exactly the 10 contract fields or that is missing any of them, before rebuilding. The shared schema validator now uses the same `REPORT_FIELDS` constant, so the exact contract is enforced in one place for both the raw and canonical checks. `validate_report_schema` behavior is unchanged.

Tests added: `opencode_normalization_rejects_extra_top_level_fields` and the end-to-end `opencode_extra_report_fields_are_rejected` via a fake CLI mode with an `"extra":1` field (status `invalid_report_schema`, `report=null`).

## Verification

Commands run on the final code (all from the repository root):

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo test --bin temote-mcp "cli::codex::delegation"` | 72 passed, 0 failed |
| `cargo test --bin temote-mcp opencode` | 63 passed, 0 failed |
| `cargo test --bin temote-mcp codex` | 132 passed, 0 failed |
| `cargo test --all-targets --all-features --locked` | 624 bin + 40 lib + 9 e2e passed, 0 failed (1 ignored, pre-existing) |
| `(cd gateway && npm test)` | 60/60 passed |
| `git diff --check` | clean |

Live read-only sanity check after the stricter normalization: one-shot `delegate --backend opencode` with the installed OpenCode and `opencode/mimo-v2.5-free` returned `status=success`, report `completed`, `artifacts_truncated=false`, and left the disposable working directory unchanged.

## Remaining limitations

- `summary` still truncates long answers at 1200 chars by design; the ` …[truncated]` marker makes the truncation explicit, but full review/planning text beyond that bound is not delivered.
- Reports whose fields are individually within schema bounds but whose canonical JSON exceeds 4096 bytes still fail as `oversized_report`; this is the existing bounded parent/report contract, not silent data loss.
- Persistent OpenCode server/session/resume remains out of scope.

## Temote verification

session: `temo`
repository: `/Volumes/DevSSD/Developer/local-mcp`
Verify:
1. read this report
2. `git status --short`
3. `git rev-parse HEAD`
4. `git rev-parse origin/main`
5. `git log -5 --oneline`
