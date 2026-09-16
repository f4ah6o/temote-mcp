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
