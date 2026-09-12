# OpenCode explicit session resume

The first session/resume slice is repository-local and uses no provider credentials. `delegate --backend opencode --session <id>` now performs a bounded read-only `opencode session list --format json` preflight before creating delegation artifacts or launching `opencode run`.

- Session IDs are bounded, ASCII, and reject whitespace, control characters, leading dashes, NULs, and path-like values.
- Exactly one matching session is required.
- The session metadata `directory`, canonicalized by Temote, must equal the canonical delegation cwd.
- Non-zero probes, timeouts, malformed or oversized output, missing IDs, duplicate IDs, missing directories, and directory mismatches fail closed.
- The resume flag is rejected for Codex. `--continue`, `--fork`, and `--attach` remain out of scope.
- The existing parent result shape is unchanged; the observed OpenCode session/thread ID remains evidence only.

Deterministic fake-CLI coverage is in `src/delegation/opencode.rs` for validation, exact argv ordering, matching-directory success, missing/duplicate/missing-directory/mismatched/malformed preflight results, non-zero and timed-out probes, oversized stdout/stderr, and the no-run/no-artifact preflight boundary. No live OpenCode session was resumed in this slice because that requires an installed, authenticated runtime and is not a repository-local acceptance gate.
