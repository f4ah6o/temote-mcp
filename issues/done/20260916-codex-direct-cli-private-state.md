# Give supported direct Codex probes bounded private mutable startup state

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Source issue: `issues/closed/20260916-codex-direct-cli-path-alias-readonly-noise.md`
Depends on: Phase 0 OpenCode canary

## Goal

Remove or explicitly classify the direct-Codex PATH-alias read-only startup warning without making host Codex/Vite+ state writable.

## Work packet

1. Add a bounded diagnostic/fixture that identifies the attempted mutable startup path for Vite+-managed and standalone Codex shapes.
2. Reuse an existing private-state abstraction if one already fits; otherwise add the smallest private writable state root for the supported direct probe path.
3. Keep credentials/config read-only and outside ordinary output.
4. Add fixtures for Vite+ and standalone behavior.

Do not stderr-filter the warning. Do not expose host `~/.codex` or `~/.vite-plus` as writable.

## Acceptance

- supported direct version/status probe has no unexplained read-only warning, or returns a fixed environment classification;
- only the necessary private state is writable;
- Vite+ and standalone fixtures pass;
- `local_agent_run` auth/state isolation tests do not regress;
- `just sandboxed-check` passes.

## Implementation notes (2026-09-16)

Current `main` did not already cover this: ordinary sandboxed commands inherited only `PATH`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, `HOME`, `XDG_CACHE_HOME`, `GOCACHE`, and Go proxy variables. `CODEX_HOME` was not forwarded, so Codex resolved `$HOME/.codex`, whose `tmp/arg0` PATH-alias directory is read-only inside the sandbox and produced `WARNING: proceeding, even though we could not create PATH aliases: Read-only file system (os error 30)`.

Changes (`src/sandbox.rs`):

- Added `apply_codex_private_state_environment`, called from `safe_environment` after the standard cache/Go remapping. When the host `$HOME/.codex` is a real directory, it creates a 0700 `<command-cache>/codex-home/tmp` under the existing per-command private cache and sets `CODEX_HOME` there, so the attempted mutable startup path (`$CODEX_HOME/tmp/arg0`) is writable for the command only.
- Credentials/config stay read-only: `auth.json` and `config.toml` are exposed as symlinks to the host files (no byte copy), only when they are regular non-symlink files. The host `$HOME/.codex` and `$HOME/.vite-plus` are never writable.
- When `$HOME/.codex` is absent, the helper is a no-op and creates nothing.
- `run_local_agent` / `run_developer_tool` keep their own environment paths; the AgentState auth-import isolation is untouched.

Fixtures (`sandbox::generic_tests`, run by `just sandboxed-check`):

- `command_environment_redirects_codex_startup_state_for_both_launcher_shapes` covers standalone and Vite+-managed (`$HOME/.vite-plus/bin/codex`) shapes: the `$CODEX_HOME/tmp/arg0/codex` write succeeds inside the private cache, the host `.codex/tmp/arg0` is never created, `auth.json`/`config.toml` remain symlinks into the host home with host bytes unchanged, and no environment value references `.vite-plus`.
- `command_environment_leaves_codex_home_untouched_without_host_state`: no host `.codex` means no `CODEX_HOME` and no cache directory.
- No stderr filtering is used, and no host Codex state is exposed as writable.

Gates:

- `cargo test --lib sandbox::generic_tests --locked`: PASS (32/32, including both new fixtures).
- `cargo test --lib local_agent --locked`: PASS (10/10).
- `cargo test --bin temote-mcp local_agent --locked`: PASS (42/42) — `local_agent_run` auth/state isolation did not regress.
- `just sandboxed-check`: exit 0 after `cargo fmt` (lib 112/112, gateway 70/70, focused bin subsets PASS).
- A first `fmt-check` failure was only the new code needing `cargo fmt --all`; no functional failure.
- live/rebuilt-runtime check of an actual sandboxed `codex --version`: NOT RUN (requires the rebuilt Temote runtime and a real managed normal session); the repository-local fixtures above are the deterministic evidence for this packet.

## 2026-09-16 completion

All repository-local acceptance items are met. The source issue `issues/closed/20260916-codex-direct-cli-path-alias-readonly-noise.md` is already superseded by this packet and stays closed. The remaining rebuilt-runtime probe is folded into the Phase 0/4 rebuilt-runtime acceptance work, not this repository-local packet.
