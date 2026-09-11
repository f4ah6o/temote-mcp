# OpenCode delegation persistent session/resume pre-implementation spike

Status: investigation/design only. No production session/resume behavior was implemented.

## Result

- baseline HEAD: `2d4a878d3f138853f059336d30f256516f81bbec`
- tested OpenCode version: `1.18.30` (installed CLI, direct `opencode run`, no Temote code changes)
- final git status: tracked worktree clean except the pre-existing untracked `.worktrees/` (untouched); this spike adds only this report and an issue status update
- recommendation: implement **explicit caller-supplied `--session <id>` only**, guarded by a fail-closed session-directory preflight; reject `--continue`, `--fork`, and `--attach` in the first slice

## Existing invariants that must survive the next slice

The following landed behaviors are non-negotiable for the future implementation (verified against `src/delegation/mod.rs`, `src/delegation/opencode.rs`, and the `docs/evaluations/opencode-*20260912.md` reports):

- one Temote request = one bounded child process; prompt, stdout/stderr/events, evidence, and final report stay bounded; the canonical parent/report budget (4096 bytes) is unchanged
- canonical working directory is resolved before launch and passed as `--dir`; the child must not operate outside it
- child environment stays default-deny (`env_clear` + OpenCode allowlist); no host-wide passthrough
- executable resolution stays the shared resolver (`TEMOTE_OPENCODE_BIN` precedence over PATH, fail closed on invalid override); no caller-controlled arbitrary executable
- requested values come from caller `Options`; observed values come only from structured adapter evidence; no copying requested→observed and no model self-report trust
- deterministic status/error classification; non-zero child exit, missing/malformed/oversized report handling unchanged
- artifact cleanup on pre-result failure; artifacts retained only on result-bearing paths
- legacy `temote-mcp codex delegate` and the frozen parent JSON shape remain compatible; `local_agent_run` is not genericized or coupled

## Experiments performed (read-only, disposable directories)

All runs used the installed OpenCode `1.18.30` with model `opencode/mimo-v2.5-free` (except E4) from disposable directories `dirA`/`dirB`/`dirC`, each containing a distinct marker file. No repository file was touched. Prompts asked only to read a marker and/or recall a generated dummy code; outputs were captured in temporary files outside the repository and only bounded, non-secret facts are recorded here.

| # | Experiment | Result |
| --- | --- | --- |
| E1 | fresh `run --pure --format json --dir dirA` | exit 0; session `ses_f6d25feb3ffeQ7JciuPif0qsx0` created; marker read `ALPHA-4821`; dummy code `ORANGE-1234` remembered |
| E2/E2b | explicit `--session <S1>` with `--dir dirB` (session created in dirA) | **hang**: after `step_start`/step processing and debug `exiting loop`, no text event and no exit; killed at 150 s and 60 s (two attempts, deterministic) |
| E3 | explicit `--session <S1>` with `--dir dirA` (same dir) | exit 0; same session ID; context code `ORANGE-1234` recalled |
| E4 | explicit resume of S1 with a different model (`opencode/big-pickle`) | exit 0; same session ID; context recalled |
| E5 | `--session <S1> --fork` (same dir) | exit 0; **new** session `ses_f6d1d05b5ffe7SP6vA6teK8F5X`; context carried over |
| E6a | `--continue` in dirA (project has history) | exit 0; continued the most recent session (the E5 fork); context recalled |
| E6b | `--continue` in dirC (no prior session in that project) | exit 0 and **silently created a new session** (`ses_f6d1cbcbeffe57laoTybFOKSD1`) with no prior context; no error |
| E7 | `--session <nonexistent>` | exit 1; stderr `Error: Session not found`; no events |
| E8 | `--session "not a session"` (malformed) | exit 1; same `Session not found`; no events |
| E9 | `--fork` without `--session`/`--continue` | exit 1; `--fork requires --continue or --session` |
| E10a | resume a `--pure`-created session without `--pure` | exit 0; same session ID; context recalled |
| E10b/E10c | create without `--pure`, resume with `--pure` | exit 0; same session ID; context recalled |
| E14 | `run --attach http://127.0.0.1:9 ...` (no server) | exit 1; `Session not found` (attach has no local server/session fallback without a session; requires server lifecycle + auth) |
| E15 | `session list --format json` from dirA and dirB | global list; both directories returned the same 22 sessions, including S1; each entry exposes `id`, `directory`, `projectId`, `title`, `created`, `updated` |
| E16 | one-shot session persistence | the E1 session created by an ordinary one-shot run appears in `session list`; sessions are persisted even for one-shot delegation |
| E17 | `run --help` surface | no ephemeral/no-session/delete-on-exit flag; `--title` defaults to a truncated prompt; `--share`/`--auto`/interactive flags exist and must never be used by Temote |

## Bounded representative event shapes

- Fresh run with tool use: event types `step_start`, `tool_use`, `step_finish`, `text` (one assistant step).
  `text` event top-level keys: `type`, `timestamp`, `sessionID`, `part`; `part` keys: `id`, `messageID`, `sessionID`, `type`, `text`, `time`.
- Resume run: `step_start`, `text`, `step_finish`; same top-level shape; `sessionID` equals the requested session ID.
- Fork run: same shape with a new `sessionID`.
- No `modelID`/`providerID`/`model` field appears in any inspected event, consistent with the earlier observed-evidence finding. With `--print-logs --log-level DEBUG`, stderr contains structured `stream providerID=... modelID=... session.id=...` lines, but that is a log surface, not the JSON event contract; Temote should not parse it in this slice.

## cwd-binding findings

- With `--dir dirA` and session S1 (created in dirA), resume works and operates in dirA.
- With `--dir dirB` and session S1 (created in dirA), OpenCode resolves the instance to the session's original directory: debug logs show `watcher backend directory=dirA` and `booting location services directory=dirA` despite `--dir dirB`. The model step runs, the loop exits, and the process then hangs without emitting the final event or disposing.
- This means a mismatched resume is not just an error: it runs with the session's original project directory (ignoring the caller's canonical cwd) and then fails to terminate. Temote must not launch a resume unless it has verified the session belongs to the canonical cwd.
- `session list --format json` exposes a per-session `directory` field; for S1, that value canonicalizes exactly to dirA. That surface can be used for a fail-closed preflight, with strict bounds and no disclosure.

## State / privacy implications

- Ordinary one-shot runs already persist sessions in OpenCode's data store; there is no ephemeral flag in `1.18.30`. Resume makes that state actionable.
- `session list --format json` returns `title` (defaults to a truncated prompt) along with session IDs and directories. A preflight must parse only bounded `id`/`directory` fields, never log or persist titles or other sessions, and must fail closed on oversized/unexpected output.
- `--share`, `--auto`, `--interactive`, `--title`, and the username/password options are out of scope and must never be added by Temote.

## Recommended first implementation slice

Support **explicit caller-supplied session resume only**:

```
temote-mcp delegate --backend opencode --session <session-id> --model <provider/model> [...prompt options]
```

Fail-closed preflight before any child `run` is spawned:

1. Validate the session ID: non-empty, NUL-free, ≤256 bytes, no ASCII whitespace/control characters, must not start with `-`. Invalid → bounded usage error (exit 2), no child.
2. Resolve the OpenCode executable with the existing shared resolver and resolve the canonical cwd exactly as today.
3. Run a bounded, read-only `opencode session list --format json` probe with the same minimized environment/allowlist, the same executable, bounded stdout, bounded timeout, child cleanup, and no artifacts.
4. Parse bounded JSON and require an entry whose `id` equals the requested ID and whose `directory`, canonicalized, equals the canonical cwd. Missing entry, directory mismatch, probe non-zero exit, invalid/oversized output, or timeout → fail closed with a bounded `OpenCode session unavailable: ...` error before launching `run`.
5. Only then append `--session <id>` to the existing argv, unchanged otherwise:

```
opencode run --pure --format json --dir <canonical cwd> --model <m> [--variant <v>] --session <id> -- <wrapped prompt>
```

Contract decisions:

- requested session: caller input, validated; not copied into observed evidence and not serialized in the frozen parent result in this slice (a parent-shape extension would need a separately reviewed compatibility change).
- observed session: remains `evidence.thread_id`, sourced only from structured events. If the observed session differs from the requested one, record the observed value as-is; do not infer or overwrite.
- report normalization, requested-vs-observed model rules, artifact handling, and status taxonomy are unchanged.
- `--session` is OpenCode-only; the Codex backend rejects it with a bounded usage error (`--session is only supported by the opencode backend`), mirroring `--variant`.
- `--continue`, `--fork`, and `--attach` are not exposed in this slice.

## Rejected / deferred alternatives

- `--continue` (rejected): implicitly selects the most recent project session (E6a) and silently creates a new session when the project has no history (E6b, exit 0). Not fail-closed and not traceable to a caller choice.
- `--fork` (deferred): works and returns a new session ID with inherited context (E5), but adds branch-state creation semantics and would need the same directory preflight plus parent/fork evidence design.
- `--attach` / `opencode serve` (deferred): requires server lifecycle, port/auth handling, and session ownership on the server (E14). Out of scope and adds long-lived process state.
- Implicit resume from previously observed `thread_id` (rejected): the expected default and consistent with "do not infer observed values"; Temote must not silently reuse state.
- Reading OpenCode session storage directly (rejected): private implementation detail; the CLI `session list --format json` surface is the only supported probe.

## Production files expected to change next

- `src/delegation/mod.rs`: `Options.session: Option<String>`; parse `--session`; Codex rejection; pass-through.
- `src/delegation/opencode.rs`: session ID validation, session-list preflight (bounded subprocess + bounded JSON), argv change, bounded `OpenCode session unavailable` error classification.
- `docs/usage.md`, `docs/usage.ja.md`: document `--session` and the fail-closed directory check.
- a new `docs/evaluations/…` report for the implementation slice.

## Deterministic tests to add next

- argument parsing: `--session` accepted with OpenCode; rejected for Codex; empty/whitespace/NUL/overlong/leading-dash rejected before launch.
- argv construction: exactly one `--session <id>` inserted before `--`, otherwise byte-identical argv.
- preflight: matching session+directory proceeds; missing session fails before spawn; directory mismatch fails before spawn; probe non-zero/invalid JSON/oversized/timeout fails closed; probe uses the same minimized environment and resolved executable.
- e2e with a fake OpenCode session-list + run script: successful resume normalizes the report; `evidence.thread_id` is the observed session; requested session is not serialized (frozen parent shape fixture unchanged); observed session different from requested is recorded as observed.
- no child `run` and no artifact directory on preflight failure (reuse the artifact-cleanup test counter pattern).
- Codex compatibility: existing frozen parent JSON fixture and `codex delegate` tests unchanged; `codex delegate --session` errors.

## Unresolved blockers

- The cross-directory hang is upstream OpenCode `1.18.30` behavior; Temote's only safe option is preflight refusal. The preflight depends on the `session list --format json` field shape, which is not a documented stable contract; the implementation must fail closed if that shape changes.
- Privacy: the preflight transiently reads session metadata. It must parse only bounded `id`/`directory` and never persist, log, or report other sessions or titles.
- Session IDs have no documented grammar; validation stays bound-based and OpenCode remains the authority for existence.
- If a product decision requires the requested session ID in the parent result, that is a separate parent-shape compatibility change.

## Temote verification

session: `temo`
repository: `/Volumes/DevSSD/Developer/local-mcp`
Verify:
1. read this report
2. `git status --short`
3. `git rev-parse HEAD`
4. `git rev-parse origin/main`
5. `git show --stat --oneline HEAD`
6. confirm `src/` is unchanged by this spike (only this report and the issue status update are added)
