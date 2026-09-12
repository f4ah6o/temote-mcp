# Codex delegation evaluation

Status: implementation and fake-transport verification complete; one-shot Codex and OpenCode delegation backends, read-only OpenCode diagnostics, and backend adapter extraction landed on main; live Codex vs OpenCode comparison recorded in `codex-vs-opencode-live-20260912.md`; OpenCode normalized-report delivery fix verified in `opencode-normalized-report-fix-20260912.md`; app-server comparative work remains pending.

## Current evidence

- The delegation implementation is split under `src/delegation/`: `mod.rs` keeps backend-neutral orchestration (backend selection, CLI parsing, options/result/evidence types, artifact and wait/capture policy, report validation, serialization), `codex.rs` owns Codex argv/environment/JSONL parsing and validation, and `opencode.rs` owns the one-shot `opencode run` adapter plus the read-only diagnostics probes. External CLI, JSON, and status behavior is unchanged.
- `codex delegate` uses `codex exec --ignore-user-config --ephemeral --sandbox workspace-write`, bounded JSONL/stderr capture, a schema-validated final report, and filtered usage fields.
- `delegate --backend opencode` uses one-shot `opencode run --pure --format json --dir <cwd> --model <provider/model> [--variant <variant>] -- <prompt>` with the same bounded artifact capture and report schema. The prompt is passed as one argv element (no shell); the child environment is rebuilt from a small OpenCode allowlist; `opencode` is resolved from PATH and callers cannot supply an executable path. JSON events are normalized to the same parent result: the last assistant message's text parts form the final report, `step_finish.part.tokens` provides usage, and top-level `sessionID` becomes the bounded thread/session evidence. A bounded timeout kills the child and returns `process_timeout`.
- `delegate diagnose --backend opencode [--model <provider/model>]` prints read-only bounded JSON readiness for the OpenCode CLI: executable resolution and version, `opencode models --pure` discovery status/count/truncation, and `present`/`absent`/`unknown`/`not_checked` requested-model validation. It never logs in, mutates credentials, downloads models, or runs a delegation task, and it maps unavailable, failed, timed-out, empty, or truncated model discovery to `unknown` instead of a false `absent`.
- `TEMOTE_OPENCODE_BIN` optionally selects the OpenCode executable: an absolute path to an existing executable regular file (symlinks are canonicalized), taking precedence over PATH. An invalid explicit override fails closed without PATH fallback. Delegation and diagnostics share one resolver, the path is never printed, and diagnostics report only the source (`env_override`/`path`/`invalid_override`) plus a bounded reason.
- The generic `temote-mcp delegate --backend codex|opencode ...` command selects the backend (explicit flag, then `TEMOTE_DELEGATION_BACKEND`, then Codex); the legacy `temote-mcp codex delegate ...` command always forces Codex.
- The app-server adapter uses local stdio, an exact `0.153.4` compatibility check, session-instance and canonical-scope ownership, durable pre-side-effect operation receipts, child approvals independent of Temote yolo, bounded evidence, and reconciliation states. Pre-thread transient failures are retryable with the same start operation; uncertain thread/turn boundaries remain reconciliation-required. Unexpired task records, including terminal records, are retained until task retention expires; only expired terminal records without a live runtime are prunable, and a full scope rejects new starts. Compacted operation receipts fail closed on exact replay for the full task retention period.
- Rust unit/property tests, gateway contract tests, formatting, clippy, no-default-features checks, and diff checks are the repeatable verification set for this implementation.
- Live comparison of the two delegation backends (3 read-only tasks × 3 runs each, fixed prompts, blind content scoring) is recorded in [`codex-vs-opencode-live-20260912.md`](codex-vs-opencode-live-20260912.md). In that sample Codex delivered normalized results 9/9 and OpenCode 1/9, with close underlying answer quality. A follow-up adapter fix raised OpenCode to 9/9 on the same prompts; see [`opencode-normalized-report-fix-20260912.md`](opencode-normalized-report-fix-20260912.md).

## Live dogfood record (2026-09-11, macOS)

Codex delegate: Codex CLI `0.153.4` installed through Vite+ (`vp`), existing Codex authentication present. Command run from a disposable git repository with one base commit:

```sh
temote-mcp codex delegate \
  --model gpt-5.6-luna \
  --reasoning-effort high \
  --prompt 'Create hello.txt containing exactly: ok ...'
```

Result (non-secret fields):

- parent result `status=success`, child exit code `0`, `artifacts_truncated=false`;
- requested model/effort: `gpt-5.6-luna` / `high`;
- observed model/effort: `null` (the child event stream did not expose them; requested values were not copied into observed);
- evidence: `thread_id` present; usage fields captured as `input_tokens=50442`, `cached_input_tokens=32256`, `output_tokens=512`, `reasoning_output_tokens=151`;
- child report `status=completed`, summary `Created hello.txt containing ok.`, one changed file, evidence artifact paths bounded and private;
- worktree verification: `hello.txt` was created in the delegated repository and `README.md` was untouched;
- no credential values, prompts, transcripts, or raw logs are recorded here.

OpenCode delegate: OpenCode CLI `1.18.30`, command run from an empty disposable directory with a free model and a read-only task:

```sh
temote-mcp delegate \
  --backend opencode \
  --model opencode/mimo-v2.5-free \
  --prompt 'Do not create, modify, or delete any files. Reply with a short completion report.'
```

Result (non-secret fields):

- parent result `status=success`, report `status=completed`, child exit code `0`, `artifacts_truncated=false`;
- requested model: `opencode/mimo-v2.5-free`; observed model/effort: `null` (not exposed by the event stream);
- evidence: `session_id` captured as `thread_id`; usage mapped as `input_tokens=10647`, `cached_input_tokens=0`, `output_tokens=346`, `reasoning_output_tokens=0`, `total_tokens=10993`;
- the delegated directory contained no created, modified, or deleted files;
- no credential values, prompts, transcripts, or raw logs are recorded here.

OpenCode diagnostics: OpenCode CLI `1.18.30`, read-only probes only (`opencode --version`, `opencode models --pure`):

```sh
temote-mcp delegate diagnose --backend opencode --model opencode-go/deepseek-v4-flash
```

Result (non-secret fields):

- `executable=available resolved=true`, `version=ready 1.18.30`, `models=ready count=64 truncated=false`;
- `requested_model=present opencode-go/deepseek-v4-flash`; without `--model` the status is `not_checked`;
- no login, auth, config, or model-cache mutation and no delegation process; diagnostics temp artifacts are removed.

Post-extraction parity re-run (2026-09-11, `src/delegation/{mod,codex,opencode}.rs`): the same read-only one-shot command and model returned `status=success`, report `status=completed`, child exit code `0`, `artifacts_truncated=false`, and a read-only disposable directory with no created, modified, or deleted files; `delegate diagnose --backend opencode` returned the same shape for both `opencode-go/deepseek-v4-flash` and `opencode/mimo-v2.5-free` (`present`, `version=1.18.30`, `models=ready count=64`).

Limitations observed: the Codex child's `changed_files` entry was an absolute artifact path rather than a repository-relative path, and child-reported `requested_model`/`requested_effort` fields were empty; the parent result keeps requested and observed values distinct. This is child report content, not a Temote serialization defect.

## Remaining live work

App-server dogfood and the direct-Temote / `codex exec` / app-server comparison remain. Run those with the same base commit, permissions, and acceptance criteria, then record observed model/effort, usage source, process outcome, elapsed times, retries, and parent intervention. Do not infer token or cost savings from MCP response bytes. The direct Codex-vs-OpenCode comparison is recorded in [`codex-vs-opencode-live-20260912.md`](codex-vs-opencode-live-20260912.md).

## Increment attempt (2026-09-13): live app-server dogfood blocked

Attempted from `main` at `091529db1b69d7e2b608ea60761a63d56dccb7b9`. The execution environment available to this increment exposed no shell/process-execution capability, so no Codex or `temote-mcp` process could be launched. The following remain unperformed, and every associated field is `unknown`; no value was inferred:

- installed Codex version, `codex app-server --stdio` handshake, and `model/list` for `gpt-5.6-luna` with `max`/`high`;
- one real `codex_task_start` dogfood and any `codex_task_get` status/usage/resume/control evidence;
- the direct-Temote / `codex exec` / app-server comparison under identical base, permissions, and acceptance criteria;
- the `AGENTS.md` local checks (format, Rust tests, clippy, no-default-features, gateway tests, `git diff --check`).

Static verification only (not live evidence): `src/codex_app_server.rs` pins the initialize handshake to `codex_cli_rs/0.153.4` (`validate_initialize_response`), `codex_status` issues `model/list` with `includeHidden:true` and returns each model's `supportedReasoningEfforts`, `codex_task_start` validates the requested model/effort against that advertised list before `thread/start`, control is limited to typed `steer`/`resume`/`interrupt`, and the four tools are registered in `src/mcp.rs`. That pass exposed no implementation defect, so no code change was made in this attempt; a follow-up schema review below found and fixed a real protocol mismatch.

Adoption decision for this increment: unchanged / undecided pending live evidence. This attempt does not satisfy Phase D or the live acceptance criteria, and it does not alter the existing one-shot `codex delegate` evidence.

## Offline schema review (2026-09-13): `thread/start` sandbox value defect

A follow-up static review compared the app-server request parameters against the captured 0.153.4 protocol schemas (`.artifacts/codex-app-server-schema-0.153.4`, a local reference not committed here). Both `thread/start` and `thread/resume` were sending `"sandbox": "workspaceWrite"`, but the 0.153.4 wire `SandboxMode` enum is `read-only` / `workspace-write` / `danger-full-access`. The camelCase value is rejected by the real app-server, so `codex_task_start` (and runtime re-establishment through `thread/resume`) could not complete against a live host. This is a separate field from `turn/start`'s `sandboxPolicy.type`, which is correctly the camelCase `workspaceWrite`.

Fixed in `src/codex_app_server.rs`: both `thread/start` and `thread/resume` now send `"workspace-write"`. The fake app-server transport now rejects any `thread/start`/`thread/resume` `sandbox` other than `workspace-write`, so the mismatch cannot silently recur in fake-transport tests. This fix is offline-verified only; live app-server dogfood and the exec/app-server comparison remain pending.

## Offline schema review (2026-09-13): `reasoningEffort` field mismatch

The same 0.153.4 schema review found a second wire mismatch in the effort-compatibility path. `model/list` returns `data[].supportedReasoningEfforts[]` as `ReasoningEffortOption` objects with a `reasoningEffort` field (`v2/ModelListResponse.json`, `v2/ReasoningEffortOption.json`), but `validate_model_request` and `codex_status` read `effort` (or a bare string). Against a real 0.153.4 app-server every requested effort would have been reported as not advertised, so `codex_task_start` would terminate as `failed`, and `codex_status` would list empty effort sets. The fake app-server fixtures also emitted `effort`, so fake-transport tests could not catch it.

Fixed in `src/codex_app_server.rs`: a single `advertised_effort_name` parser reads `reasoningEffort` first and keeps `effort`/bare-string tolerance; the fake app-server fixtures now emit the real `reasoningEffort` shape; a regression test pins the parser and the negative case. No public tool argument, approval, sandbox, retention, or evidence behavior changed. This fix is offline-verified only; live app-server dogfood and the exec/app-server comparison remain pending.
