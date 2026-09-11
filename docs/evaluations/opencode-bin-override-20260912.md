# OpenCode executable override

## Result

- status: landed on main
- baseline: `9486334ff90f33c5b1efcf2b1538413aa5e96a22`
- implementation commit: `93c07b10ce85dc8de5cad4cf31abec1fd6a67714`
- report commit: `7ccdfae79489440b93b1244ca890e5eeb5c3d0ea`
- final HEAD: the status-only finalization commit listed in `git log -5 --oneline` (no code or content changes in it); use `git rev-parse HEAD` for the exact hash
- pushed: yes (two pushes: implementation + docs, then the status finalization commit)
- tracked worktree clean: yes (`?? .worktrees/` only, untouched)

## Contract

- precedence: `TEMOTE_OPENCODE_BIN` → PATH-resolved `opencode` → unavailable
- accepted value: one absolute executable path. The value is used verbatim as a path; it is never shell-parsed, so spaces or fragments are not interpreted, and no argv can be injected. Bounds: non-empty, NUL-free, ≤4096 bytes, absolute, exists as a regular file after symlink canonicalization, and executable.
- invalid override behavior: fail closed with a bounded error that names only `TEMOTE_OPENCODE_BIN` and the reason; no RESULT is produced and no PATH fallback happens.
- PATH fallback behavior: only when `TEMOTE_OPENCODE_BIN` is unset. An explicit but invalid override never falls back to a different PATH executable.
- path disclosure policy: the resolved physical path is never printed in diagnostics, results, or errors. Diagnostics report only `status` (`available`/`unavailable`), `resolved`, `source` (`env_override`/`path`/`invalid_override`), and a bounded `reason` for invalid overrides. The environment value is read by the parent process only and is not passed to the OpenCode child environment.

## Implementation

- resolver: `resolve_opencode_executable(override_value, fallback)` returns a private `ResolvedOpenCodeExecutable { source, binary }`. `bin_override_value()` reads `TEMOTE_OPENCODE_BIN` (a non-unicode value is an invalid override, not an unset variable). Validation order: empty → NUL/invalid → length → absolute → `fs::canonicalize` (missing) → regular file → executable bit on Unix. Symlinks are allowed and resolved to their canonical target.
- delegation integration: `parse_generic_args` resolves the executable only for the OpenCode backend; the Codex branch never reads the variable, and legacy `codex delegate` is untouched. Resolution happens during argument parsing, before any child or artifact is created. Errors are rendered as `OpenCode backend unavailable: <reason>` plus usage, exit code 2. The launch error no longer prints any executable path.
- diagnostics integration: `diagnose_default()` uses the same resolver. An invalid override produces deterministic diagnostics without probing: `executable.status=unavailable`, `source=invalid_override`, bounded `reason`, and `version`/`models` `unavailable` with requested-model status `unknown`/`not_checked`.
- security: no generic executable surface was added. There is no CLI flag, no `TEMOTE_AGENT_BIN`, no shell override, and no path input from prompts or MCP requests. The existing child environment allowlist is unchanged and does not include `TEMOTE_OPENCODE_BIN`.

## Tests

- focused OpenCode adapter: `cargo test --bin temote-mcp "delegation::opencode::tests"` — 52 passed (8 new in this slice), repeated 3× without flakes
- shared delegation module: `cargo test --bin temote-mcp "delegation::tests"` — 7 passed (3 new: Codex unaffected, valid override, invalid override)
- delegation overall: `cargo test --bin temote-mcp "cli::codex::delegation"` — 67 passed
- diagnostics: `cargo test --bin temote-mcp diagnostics` — 15 passed
- Codex parity: `cargo test --bin temote-mcp codex` — 127 passed
- cargo fmt: pass (`cargo fmt --all -- --check`)
- cargo clippy: pass (`cargo clippy --all-targets -- -D warnings`)
- cargo check: pass (`cargo check --no-default-features --all-targets`)
- full cargo test: pass (`cargo test --all-targets --all-features --locked`; 619 bin tests + 40 lib + e2e suites, 0 failures)
- gateway npm test: pass (60/60)
- git diff --check: pass

New deterministic tests cover: valid override wins over fallback; unset → PATH lookup; empty, relative, missing, directory, non-executable, embedded-NUL, and overlong overrides rejected without fallback; symlink canonicalization; invalid-override diagnostics source/reason; `env_override` and `path` source labeling; error messages and delegation errors not leaking a sentinel path; `TEMOTE_OPENCODE_BIN` filtered out of the child environment; and the Codex backend unaffected by an invalid override.

## Smoke

- env override valid: with the installed OpenCode path supplied through `TEMOTE_OPENCODE_BIN`, `delegate diagnose --backend opencode --model opencode-go/deepseek-v4-flash` reported `source=env_override`, `status=available`, `version=ready 1.18.30`, `models=ready count=64`, `requested_model=present`. A read-only one-shot delegation through the override returned `status=success`, report `completed`, and left the disposable directory unchanged.
- invalid override: a relative value diagnosed `source=invalid_override`, `reason=not_absolute`; an absolute nonexistent value diagnosed `reason=not_found`. Delegation with the nonexistent override exited non-zero with `OpenCode backend unavailable: TEMOTE_OPENCODE_BIN does not point to an existing file`, produced no result JSON, and did not run the PATH-installed OpenCode.
- PATH fallback: with `TEMOTE_OPENCODE_BIN` unset, diagnostics reported `source=path`, `status=available`.
- physical path leaked: no. Sentinel-path checks confirmed diagnostics, errors, and delegation output do not contain the configured path.

## Remaining

- persistent server/session/resume

## Temote verification

session: `temo`
repository: `/Volumes/DevSSD/Developer/local-mcp`
Verify:
1. read this report
2. `git status --short`
3. `git rev-parse HEAD`
4. `git rev-parse origin/main`
5. `git log -5 --oneline`
