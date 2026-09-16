# Implement safe session metadata orphan GC with dry-run and drift revalidation

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Source issue: `issues/closed/20260916-session-metadata-orphan-gc.md`
Depends on: Phase 0 OpenCode canary

## Goal

Add a bounded maintenance path for old `invalid_orphan` metadata without deleting live, ambiguous, malformed, symlinked, or protected session state.

## Scope

Implement only the initial reviewed candidate classes: `missing_json` and `missing_state` after the documented grace period.

Required behavior:

- dry-run performs zero filesystem mutation;
- deterministic bounded ordering/limit;
- exclude live, supervisor-owned, upgrade-protected, malformed, symlink/special-file, metadata-ID-mismatch entries;
- apply revalidates state after preflight and skips/fails closed on drift or concurrent start;
- cleanup regression covers session list/info and supervisor restart behavior.

Do not expand the candidate set in this packet.

## Acceptance tests

Add focused tests for dry-run, grace boundary, protected/live exclusion, unsafe file types, ID mismatch, drift, concurrency, ordering, and limit. Then run format, strict clippy, no-default check, focused tests, `git diff --check`, and `just sandboxed-check`.

## Implementation notes (2026-09-16)

Current `main` had no GC path at all (the earlier dirty five-file draft was intentionally abandoned; fresh implementation). Changes:

- `src/session_control.rs`: added `SessionGcReason` (`missing_json` / `missing_state`), `SessionGcEntry` / `SessionGcReport`, a `24h` grace constant, limits (`1..=1000`), a deterministic plan builder (`oldest mtime first`, then ID), an apply path with per-candidate revalidation, and a supervisor-callable `run_session_gc`. Eligibility requires: only the initial reviewed lone-half classes, regular non-symlink file, older than grace, valid session ID, not supervisor-owned, not upgrade-restore-protected, terminal stopped/crashed with `stopped_at` (lone `.state`) or readable metadata with matching ID (lone `.json`), and no live session socket probe. Deletion uses the existing no-follow `remove_owned_session_entry` on exactly one Temote-owned metadata file.
- `src/supervisor.rs`: `SessionSupervisor::gc_session_metadata` holds the transition lock for the whole plan/apply (so no managed start can race a deletion) and gates `--apply` on `ensure_mutations_allowed`.
- `src/session_control.rs` control protocol: `Gc { apply, limit }` request and `session_control::gc` client.
- `src/cli.rs` / `src/main.rs`: `temote-mcp session gc [--apply] [--limit N]`, dry-run default, limit `1..=1000` (default 100), help/usage updated.
- `src/doctor.rs`: reason counts (`missing_json`, `missing_state`) plus a WARN with `session gc` guidance when `invalid_orphan > 0`; `SessionMetadataDiagnostics` gained the reason fields, counted in `build_retention_plan`.
- `src/config.rs`: `remove_owned_session_entry` visibility only.
- `justfile`: `just sandboxed-check` now includes the `session_gc` binary tests.
- `docs/usage.md` / `docs/usage.ja.md`: operator contract for `session gc`.

Observed behavior:

- Manual doctor check with a fixture state dir: `[WARN] session metadata: entries=2 ... invalid_orphan=2 missing_json=1 missing_state=1` with the `session gc` guidance.
- `temote-mcp session --help` lists `gc` with the dry-run description.

Tests:

- `cargo test --bin temote-mcp --locked session_gc`: PASS (7/7) — dry-run read-only, apply removes only reviewed orphans, grace boundary, ordering/limit/truncation determinism, drift (touched file, counterpart appeared) and concurrent-start skip, diagnostics reason counts, bounded limit.
- `cargo test --bin temote-mcp --locked session_gc_defaults`: PASS (CLI parse/default/bounds).
- `cargo test --test session_metadata_retention_e2e --locked`: PASS (10/10) including the new `session_gc_dry_run_and_apply_remove_only_old_orphans_and_survive_restart`, which covers live supervisor ownership, dry-run/apply, `session list`/`session info`, and supervisor restart. The E2E creates the lone `.json` only after the supervisor starts because startup reconciliation materializes a lifecycle half for discovered metadata.
- `cargo test --bin temote-mcp --locked`: 861 passed, 2 failed — `codex_app_server::tests::app_server_rejects_incompatible_and_oversized_protocol` and `cross_process_store_and_runtime_ownership_are_fenced`. Reproduced with this packet's working tree stashed, so they are pre-existing main failures, recorded as evidence in `issues/polished/20260916-completion-appserver-branch-salvage.md`.
- `just sandboxed-check`: exit 0 (lib 112/112, bin subsets including `session_gc` 8/8, gateway 71/71, clippy clean under 1.98.0).
- host/CI-only: NOT RUN (actual CI run, Linux nested sandbox runtime tests, full binary/local Unix-socket integration suite, ignored supervisor/process-boundary E2E).

## 2026-09-16 completion

All packet acceptance items are met repository-locally. The source issue `issues/closed/20260916-session-metadata-orphan-gc.md` is already superseded by this packet and stays closed. The pre-existing codex app-server test failures are tracked by the Phase 3 app-server salvage packet, not this one.
