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
