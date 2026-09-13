# OpenCode explicit session resume

The first session/resume slice is repository-local and uses no provider credentials. `delegate --backend opencode --session <id>` now performs a bounded read-only `opencode session list --format json` preflight before creating delegation artifacts or launching `opencode run`.

- Session IDs are bounded, ASCII, and reject whitespace, control characters, leading dashes, NULs, and path-like values.
- Exactly one matching session is required.
- The session metadata `directory`, canonicalized by Temote, must equal the canonical delegation cwd.
- Non-zero probes, timeouts, malformed or oversized output, missing IDs, duplicate IDs, missing directories, and directory mismatches fail closed.
- The resume flag is rejected for Codex. `--continue` and `--attach` remain out of scope; `--fork` is added as a bounded follow-up below.
- The existing parent result shape is unchanged; the observed OpenCode session/thread ID remains evidence only.

Deterministic fake-CLI coverage is in `src/delegation/opencode.rs` for validation, exact argv ordering, matching-directory success, missing/duplicate/missing-directory/mismatched/malformed preflight results, non-zero and timed-out probes, oversized stdout/stderr, and the no-run/no-artifact preflight boundary. No live OpenCode session was resumed in this slice because that requires an installed, authenticated runtime and is not a repository-local acceptance gate.

## Follow-up: explicit `--fork`

The bounded `--fork` follow-up is also repository-local and uses no provider credentials. `delegate --backend opencode --session <id> --fork` starts a new OpenCode session that inherits the named session's context (verified upstream in the resume spike as experiment E5).

- `--fork` is OpenCode-only and requires `--session`; it is rejected for Codex and rejected without a session at argument parsing.
- The identical fail-closed `opencode session list --format json` directory preflight runs against the parent session before any artifact or `run` process is created.
- `run` argv appends exactly one `--fork` immediately after `--session <id>` and before `-- <prompt>`; the rest of the argv is unchanged.
- `--fork` does not change the frozen parent result shape. `evidence.thread_id` remains the observed session, which is the new forked session ID.
- `--continue` and `--attach` remain unsupported; `--attach` still requires a managed OpenCode server lifecycle.

Deterministic coverage: `fork_requires_session_and_is_opencode_only`, `fork_command_places_fork_after_session_before_prompt`, and `fork_preflight_uses_parent_session_and_launches_new_session` in `src/delegation/opencode.rs`. The upstream fork behavior itself was confirmed only in the read-only spike; a live fork remains an optional operator check.
