# Proposal: OpenCode delegation backend

- Status: Open / Phase 1, one-shot 1.18.30 OpenCode backend, diagnostics, backend adapter extraction, live comparative measurement, and normalized-report fix landed on main; `TEMOTE_OPENCODE_BIN` implemented and verified locally; persistent/session features not started
- Date: 2026-09-10 (Asia/Tokyo)
- Updated: 2026-09-12 (Asia/Tokyo)
- Priority: P1
- Original baseline inspected: `cbeb6d0dfa352c681d1d5696728f76653d5c5cd2` (`main`)
- Proposal path: `issues/open/20260910-opencode-delegation-backend.md`
- Related:
  - [TEMOTE-08: Codex delegation dogfood and app server](20260908-08-codex-delegation-dogfood-and-app-server.md)
  - [Developer Execution Broker](20260910-developer-execution-broker.md)
  - `src/codex.rs`
  - `src/delegation/mod.rs`
  - `src/delegation/codex.rs`
  - `src/delegation/opencode.rs`
  - `src/local_agent.rs`

## Current `main` boundary (2026-09-11)

PR #13 already added a structured `local_agent_run` broker for both Codex and OpenCode. That broker is a one-shot development execution capability with canonical session-root cwd checks, a fixed adapter-owned argv, bounded task/output, isolated environment/state, and a dedicated local-agent sandbox profile.

This issue does **not** reimplement that broker. Its remaining product goal is narrower and different:

> make the existing schema-validated `codex delegate` workflow backend-neutral, so OpenCode can participate in the same bounded delegation report/evidence contract and generic `temote-mcp delegate --backend ...` CLI.

The two surfaces may share low-level OpenCode command/fixture knowledge where safe, but they have different contracts:

- `local_agent_run`: structured host-side development worker execution;
- `delegate`: bounded delegated-task report/evidence protocol with requested-vs-observed model/usage semantics and compatibility with the existing `codex delegate` workflow.

Do not add a second generic local-agent executor while implementing this issue. Do not make `local_agent_run` depend on the delegation report schema merely to reuse code.

## Recommended next implementation slice

Start with **Phase 1 + a boundary inventory only**:

1. freeze current `codex delegate` behavior with compatibility fixtures/tests;
2. document which OpenCode launch/parsing helpers in `src/local_agent.rs` are reusable without importing local-agent authorization/sandbox semantics into delegation;
3. introduce backend-neutral internal result/evidence types without changing CLI output or launching OpenCode;
4. stop before the first OpenCode child process is added, so the extraction is independently reviewable.

## Motivation

Temote MCP now has a working Codex-specific delegation bootstrap, but delegation as a product capability should not be coupled to one agent CLI. The immediate goal is to make OpenCode CLI selectable as a second delegation backend while preserving the safety and reliability properties already established by Codex delegation.

The target user flow is conceptually:

```text
temote-mcp delegate \
  --backend opencode \
  --model <provider/model> \
  --prompt-file task.md
```

Existing compatibility surface should remain available during migration:

```text
temote-mcp codex delegate ...
```

A motivating live configuration is OpenCode with OpenCode Go and DeepSeek V4 Flash. The model/provider string must not be hard-coded. OpenCode documents model selection as `provider/model`, provides `opencode models` for discovery, and currently documents DeepSeek V4 Flash in OpenCode Go. The implementation must still discover or verify the effective provider/model ID on the target installation instead of assuming a stale identifier.

## External OpenCode facts verified for this proposal

Verified against OpenCode documentation on 2026-09-10:

- `opencode run [message..]` is the non-interactive execution command.
- `opencode run` supports `--format json`, described as raw JSON events.
- `opencode run` supports `--model <provider/model>` and `--dir <working-directory>`.
- `opencode models [provider]` lists currently available models using `provider/model` identifiers.
- OpenCode Go currently lists DeepSeek V4 Flash and documents OpenCode Go config model IDs in the form `opencode-go/<model-id>`.
- OpenCode also exposes session continuation, attach/server modes, and other stateful features, but those are not required for the first implementation.

References:

- https://opencode.ai/docs/cli/
- https://dev.opencode.ai/docs/go/
- https://dev.opencode.ai/docs/models/

These are discovery inputs, not a frozen protocol contract. Before implementation, pin the tested OpenCode CLI version and capture fixtures from that version. If the JSON event schema is not explicitly versioned/stable, treat it as an adapter concern and fail closed on missing required result data.

## Current Temote state and invariants

The inspected delegation module (then `src/codex_delegation.rs`, now split under `src/delegation/`) already contains important behavior that must survive backend extraction:

- prompt input is bounded (`MAX_PROMPT_BYTES`, currently 1 MiB), with regular-file and non-symlink checks for `--prompt-file`;
- stdout/event and stderr artifacts are bounded (currently 8 MiB each capture path) and excess output is drained rather than accumulated in memory;
- the parent-facing result is bounded (currently 4 KiB);
- child environment inheritance is default-deny via `env_clear()` plus an allowlist;
- current working directory is canonicalized before child launch;
- temporary artifact directories/files receive restrictive permissions on Unix;
- non-zero child exit is distinguished from missing, malformed, invalid-schema, and oversized reports;
- requested model/effort are kept distinct from observed model/effort evidence;
- JSONL evidence parsing is bounded per artifact and per line;
- the final report is schema validated instead of trusting agent prose.

The current Codex command construction is backend-specific: `codex exec`, Codex sandbox/config flags, `--json`, `--output-schema`, `--output-last-message`, Codex environment keys, and Codex event field interpretation. These pieces should move behind an adapter boundary rather than forcing OpenCode to emulate Codex.

`src/codex.rs` also contains Codex plugin integration responsibilities such as install/uninstall/status/diagnose. Those are not delegation-generic and must remain separate from the backend abstraction.

`src/local_agent.rs` now contains a separate OpenCode one-shot execution adapter. Reuse only narrowly compatible command-discovery/fixture knowledge; its host-execution authorization and sandbox contract are not automatically the delegation contract.

## Proposed architecture

Prefer a small extraction around delegation only. One possible shape is:

```text
src/
  delegation/
    mod.rs
    backend.rs
    codex.rs
    opencode.rs
  codex.rs                  # plugin integration remains Codex-specific
```

An equivalent smaller diff that preserves the same responsibility split is acceptable. The goal is not directory churn; the goal is a clear backend boundary.

### Generic delegation layer responsibilities

The generic layer should own policy and lifecycle that must behave identically across backends:

- validated prompt acquisition and maximum prompt size;
- canonical working directory and applicable unsafe-path/symlink checks;
- private artifact directory creation and restrictive permissions;
- bounded stdout/stderr/event capture and truncation reporting;
- child-process lifecycle, wait, cancellation/interrupt plumbing, and exit-code handling;
- maximum final-report size and backend-neutral report validation;
- deterministic parent-facing status/error mapping;
- cleanup behavior;
- default-deny child environment policy enforcement;
- common requested-vs-observed evidence semantics;
- parent-facing bounded serialization.

The generic layer must not know OpenCode event type names or Codex JSONL field names.

### Backend adapter responsibilities

A backend adapter should own only backend-specific behavior, for example:

- executable selection and command construction;
- model argument mapping;
- optional reasoning/variant argument mapping;
- backend-specific environment keys requested from the common allowlist mechanism;
- stdout/event parsing;
- final-message/result extraction;
- observed model/provider/variant extraction when available;
- usage/session/tool-event evidence extraction when available;
- backend-specific diagnostic probes.

A conceptual interface could be similar to:

```text
DelegationBackend
  name()
  validate_options(...)
  build_command(...)
  allowed_environment_keys()
  parse_events(...)
  extract_report(...)
  diagnose(...)
```

This is illustrative, not a requirement to use a Rust trait if an enum plus functions produces a smaller and clearer implementation.

### Environment policy

Do not move environment filtering wholly into adapters. The common layer should continue to call `env_clear()` and enforce a default-deny policy. A backend may declare the minimal additional keys it requires, but the generic launcher decides what is actually inherited.

This prevents an OpenCode adapter from accidentally changing delegation into unrestricted parent-environment inheritance. Provider credentials remain owned/configured by OpenCode; Temote should not read, print, persist, or invent provider tokens.

## CLI and configuration

### New generic command

Proposed eventual surface:

```text
temote-mcp delegate \
  --backend codex|opencode \
  --model <provider-or-backend-specific-model> \
  --prompt-file <path>
```

For OpenCode:

```text
temote-mcp delegate \
  --backend opencode \
  --model <provider/model> \
  --prompt-file task.md
```

OpenCode-specific model strings are passed through after bounded/NUL-free validation. Temote should not maintain a static catalog.

### Backend selection precedence

Recommended for the new generic command:

1. explicit `--backend`;
2. `TEMOTE_DELEGATION_BACKEND` if supported by the implementation;
3. conservative default `codex` only if backward compatibility requires a default; otherwise require an explicit backend during the experimental period.

The legacy `temote-mcp codex delegate ...` path must force the Codex backend and must not be redirected by `TEMOTE_DELEGATION_BACKEND`.

### Binary override

Support an OpenCode binary override analogous to existing testability needs:

```text
TEMOTE_OPENCODE_BIN=/path/to/opencode
```

Default lookup may use `opencode` from `PATH`, but diagnostics and parent-facing evidence should identify the resolved executable path where practical and report the observed version. Do not treat a user-supplied executable path as trusted merely because it exists; preserve existing path validation policy where applicable.

### Reasoning effort / variant

Codex currently has a required reasoning-effort input. OpenCode exposes `--variant`, and variant names are provider/model-specific. Do not bake Codex effort vocabulary into the generic backend contract.

Migration options to evaluate during implementation:

- introduce a generic optional `variant`/backend-options field while legacy Codex CLI continues mapping `--reasoning-effort` to Codex;
- or retain a common optional `requested_effort` field for compatibility but let the OpenCode adapter map it only when explicitly supported.

In either case, unsupported or unobservable values must be represented as unsupported/unknown, not fabricated.

## OpenCode backend design

### Initial execution mode

Phase 1 OpenCode support should use one-shot non-interactive execution only:

```text
opencode run \
  --format json \
  --model <provider/model> \
  --dir <canonical-working-directory> \
  <prompt>
```

Exact argument ordering and any needed isolation/config flags must be verified against the pinned CLI version during implementation. The already-merged `local_agent_run` adapter is useful evidence for the installed CLI contract, but delegation must independently verify the pinned version/arguments it relies on.

The prompt may be supplied as one argument if the CLI contract and current prompt-size bound make that safe. If a later OpenCode version gains a safer file/stdin contract, evaluate it separately. Do not write secrets into argv or temporary prompt files as a workaround.

### JSON output adaptation

Do not reuse the Codex parser by renaming fields. `opencode run --format json` should be parsed by an OpenCode adapter and normalized into backend-neutral evidence.

The implementation investigation must record actual fixtures for at least:

- successful assistant completion;
- tool execution events;
- provider/model metadata if present;
- usage/token metadata if present;
- session identifier if present;
- process error/non-zero exit;
- partial output followed by failure;
- interrupted execution.

Unknown event types should normally be ignored within bounded capture, while malformed required events or an unextractable final result must fail deterministically.

### Final report extraction

Codex currently has `--output-schema` and `--output-last-message`. OpenCode's documented `run` flags do not provide the same contract. Therefore the OpenCode adapter should not pretend those flags exist.

Preferred first implementation:

1. Temote supplies the same strict final-report instructions/schema in the delegated task prompt or a backend-specific wrapper prompt.
2. OpenCode events are parsed to identify the final assistant output using fixtures from the supported CLI version.
3. That final output is bounded, parsed as JSON, and validated by Temote's backend-neutral report validator.
4. Missing, malformed, oversized, or schema-invalid final reports map to explicit failure statuses.

If OpenCode exposes a more reliable structured result API during implementation, it may replace step 2, but the adapter boundary and validation requirements remain.

### Evidence normalization

Normalize what can actually be observed:

- requested model: always the validated caller input;
- observed model/provider: only from OpenCode output/session evidence when present;
- session ID: evidence only for the initial one-shot backend;
- usage: only fields actually emitted and validated as numeric/bounded;
- tool events: optional bounded evidence/diagnostic counts, not raw unbounded transcript;
- final result: validated backend-neutral report.

Never copy `requested_model` into `observed_model` merely because observed metadata is absent.

## Backend-neutral result contract

The parent-facing shape should converge on a backend-neutral contract such as:

```json
{
  "status": "completed",
  "backend": "opencode",
  "summary": "...",
  "base_commit": "...",
  "changed_files": [],
  "checks": [],
  "unresolved": [],
  "requested_model": "...",
  "observed_model": null,
  "usage": {}
}
```

Additional bounded evidence such as exit code, truncation flag, session/thread ID, requested/observed variant, or artifact references can remain in a common envelope if already part of the current parent result.

Migration must account for the existing Codex report schema, which currently includes `requested_effort`, `observed_effort`, and Codex-specific evidence naming. Recommended strategy:

- preserve the existing legacy Codex CLI output during the extraction phase;
- introduce backend-neutral internal types first;
- add `backend` and neutral evidence fields without silently deleting existing fields;
- only remove/rename legacy fields in a separately reviewed compatibility change.

Backend normalization is not permission to make reports larger. The current bounded parent result remains an invariant.

## Codex compatibility boundary

The following existing Codex plugin integration must remain Codex-specific and continue working independently of delegation backend selection:

- install;
- uninstall;
- status;
- diagnose.

Do not genericize all of `src/codex.rs`. Extract only the delegation concerns then concentrated in `src/codex_delegation.rs` and adjacent CLI dispatch (landed in Phase 2 as `src/delegation/{mod,codex,opencode}.rs`).

The legacy command:

```text
temote-mcp codex delegate ...
```

should remain available through the initial migration and invoke the Codex adapter with existing semantics.

## Diagnostics

Add or plan a generic diagnostic surface:

```text
temote-mcp delegate diagnose --backend opencode
```

Minimum OpenCode checks:

- configured/default `opencode` executable can be resolved and executed;
- version can be obtained;
- `opencode run` is present in help/command discovery;
- JSON output capability is present for `run`;
- model discovery command is available;
- configured model, if one is supplied for diagnosis, appears in `opencode models` or the diagnostic clearly reports that provider/model discovery could not confirm it.

Diagnostics should distinguish local CLI capability from provider/account readiness. `opencode models` may depend on local OpenCode configuration/provider state; failure there should not be misreported as "binary missing".

Diagnostics must not print provider credentials or dump unrestricted OpenCode configuration.

## Session handling

Initial scope is one Temote delegation request -> one `opencode run` child process.

Session IDs observed in OpenCode JSON may be recorded as bounded evidence, but Temote should not initially persist or resume OpenCode sessions across requests.

Future extensions may consider:

- `--session` / `--continue`;
- `opencode serve` plus `run --attach`;
- persistent OpenCode server lifecycle;
- OpenCode ACP/server API integration.

These are explicitly deferred because they add state ownership, reconnect, authentication, and lifecycle questions that are independent of proving a safe second backend.

## Safety and reliability requirements

Adding OpenCode must not weaken any of these invariants:

- bounded stdout/stderr/event artifacts;
- bounded prompt and final report sizes;
- default-deny environment inheritance with a reviewed allowlist;
- no unrestricted secret inheritance into child processes;
- canonical working directory;
- existing symlink/unsafe path checks where applicable;
- deterministic status mapping;
- explicit non-zero process exit handling;
- explicit malformed/missing/oversized result handling;
- requested vs observed model distinction;
- restrictive temporary artifact permissions;
- cleanup behavior;
- bounded event-line parsing and bounded evidence extraction;
- no raw full transcript returned to the parent merely because OpenCode emits it.

If OpenCode requires a capability that conflicts with one of these rules, the implementation must stop and propose a narrowly reviewed change instead of silently relaxing the invariant.

## Test plan

### Unit tests

Cover at least:

- backend selection and precedence;
- OpenCode command construction;
- `provider/model` pass-through and argument validation;
- canonical cwd handling;
- environment default-deny filtering and OpenCode-specific allowlist additions;
- JSON event parsing and final-result extraction;
- invalid JSON / malformed event;
- missing final result;
- non-zero process exit;
- oversized stdout/stderr/event capture;
- oversized final report;
- requested/observed model mismatch;
- absent observed model remains unknown rather than copied from requested model.

### Deterministic integration tests with fake OpenCode

Use a fake `opencode` executable selected through test-only configuration or `TEMOTE_OPENCODE_BIN`. It should cover:

- successful run with a valid final report;
- process failure;
- malformed event stream;
- huge stdout and stderr while proving bounded artifacts/draining;
- requested/observed model mismatch;
- interrupted execution;
- missing model metadata;
- missing final message;
- executable-not-found behavior.

Fixtures should be checked into the test suite and named with the OpenCode CLI version/protocol assumptions they represent. Reuse fixture facts from `local_agent_run` only when the exact pinned CLI output is identical; otherwise keep delegation fixtures separate.

### Codex preservation tests

Before extraction, add or strengthen tests that freeze the current Codex delegation behavior:

- command arguments;
- environment filtering;
- artifact bounds and permissions;
- report validation/status mapping;
- evidence extraction;
- legacy CLI behavior.

The backend abstraction is not complete if it passes OpenCode tests by changing existing Codex semantics unintentionally.

### Optional live acceptance

On a machine with OpenCode configured, run a small repository task using a model returned by `opencode models` and verify:

- selected backend is OpenCode;
- requested provider/model is passed through;
- the delegated task runs in the intended canonical repo directory;
- result is normalized to the common contract;
- observed model/provider is recorded only if OpenCode exposes it;
- artifacts remain bounded;
- no unexpected environment values appear in diagnostic evidence.

For the motivating case, discover the current OpenCode Go DeepSeek V4 Flash identifier from the installed CLI and use that exact discovered value. Do not make the test depend on a hard-coded provider/model string that may change.

## Migration strategy

### Phase 1: freeze current Codex behavior and broker boundary

- add preservation tests around current `codex delegate` command, process construction, environment, bounded artifacts, report validation, and evidence;
- record current parent-facing JSON examples as compatibility fixtures where useful;
- inventory `src/local_agent.rs` OpenCode helpers and explicitly classify them as reusable implementation detail vs local-agent-only policy.

### Phase 2: extract backend boundary

- introduce backend-neutral delegation types/lifecycle;
- move Codex-specific command construction/event parsing into a Codex adapter;
- keep legacy `temote-mcp codex delegate` behavior passing unchanged;
- do not add OpenCode yet if extraction cannot be reviewed independently.

### Phase 3: add OpenCode one-shot backend

- pin/test an OpenCode CLI version;
- implement `opencode run --format json` adapter;
- add fake CLI fixtures and deterministic tests;
- add binary override and diagnostics;
- keep provider/model configurable;
- reuse local-agent implementation only where it does not import the wrong authorization/report contract.

### Phase 4: optional live OpenCode acceptance

- discover models from the installed/configured OpenCode instance;
- delegate a small repository task;
- record requested/observed model and bounded-result evidence;
- specifically validate the OpenCode Go + DeepSeek V4 Flash use case if available in that environment.

### Phase 5: generic CLI naming migration

- add/stabilize `temote-mcp delegate --backend ...`;
- decide default backend/config precedence;
- retain `temote-mcp codex delegate` as a compatibility alias during a documented migration window;
- only consider later deprecation after downstream usage is known.

## Non-goals

This issue does not include:

- providing or installing OpenCode itself;
- managing OpenCode provider accounts or tokens;
- storing DeepSeek/OpenCode credentials in Temote;
- reimplementing the full OpenCode feature set;
- removing Codex plugin integration;
- persistent OpenCode server/session management;
- attach mode or cross-request OpenCode session persistence;
- OpenCode UI integration;
- OpenCode MCP configuration management;
- agent/session federation changes;
- replacing or broadening the already-merged `local_agent_run` broker.

## Acceptance criteria

Implementation of this proposal is acceptable only when all of the following are true:

- existing Codex delegation tests and documented behavior do not regress;
- `opencode` can be selected as a delegation backend;
- missing/unexecutable OpenCode binary fails clearly and deterministically;
- caller can provide an arbitrary bounded provider/model identifier without Temote hard-coding the catalog;
- stdout/stderr/event and final-report bounds remain enforced;
- child environment remains default-deny and reviewed;
- OpenCode result/evidence is normalized into a backend-neutral parent contract;
- requested and observed model values remain distinct;
- Codex plugin install/uninstall/status/diagnose continue to behave independently of the new backend layer;
- fake OpenCode CLI tests are deterministic and cover success, failure, malformed/huge output, mismatch, and interruption;
- the generic layer does not depend on OpenCode-specific event names or session features;
- one-shot `opencode run` is sufficient for the first shipped OpenCode backend; persistent session/server features remain optional future work;
- `local_agent_run` behavior, authorization, and sandboxing do not regress or become coupled to delegation schema requirements.

## Implementation-review questions

Resolve these from a pinned OpenCode CLI and fixtures before merging production code:

- What exact JSON event shapes identify assistant final output, model/provider, session ID, usage, tool calls, and errors?
- Is the JSON event format versioned or documented strongly enough to parse directly, or should the adapter key off a smaller verified subset?
- Which environment variables are minimally required for OpenCode itself while leaving provider credential ownership to OpenCode configuration?
- Does OpenCode create persistent session state even for one-shot `run`, and if so, is that acceptable or should Temote request/enable an ephemeral mode if one exists?
- What is the cleanest compatibility mapping between Codex `reasoning_effort` and OpenCode provider-specific `--variant` without making either concept falsely universal?

Until these are answered, unknown fields remain unknown; they are not inferred from requested values.

## Phase 3 status (2026-09-11)

One-shot OpenCode backend landed on main:

- `DelegationBackend::{Codex, OpenCode}`; `temote-mcp delegate --backend codex|opencode ...` selects the backend (explicit `--backend`, then `TEMOTE_DELEGATION_BACKEND`, then Codex). Legacy `temote-mcp codex delegate ...` always forces Codex and is not redirected by the environment variable.
- OpenCode adapter: `opencode` is resolved from PATH and callers cannot inject an executable path; the child runs `opencode run --pure --format json --dir <canonical cwd> --model <provider/model> [--variant <variant>] -- <wrapped prompt>` with argv only and no shell.
- The prompt wrapper embeds the strict final-report contract; JSON events are normalized into the existing backend-neutral parent result (last assistant message text parts for the report, `step_finish` tokens for usage, `sessionID` for bounded evidence). Requested and observed model/variant stay distinct.
- The child environment is rebuilt from a small OpenCode allowlist (no host-wide passthrough), and a bounded run timeout kills the child and returns `process_timeout`.
- Tests cover backend selection and flag validation, argv construction, cwd canonicalization, missing executable, success normalization, non-zero exit, timeout, bounded stdout, secret sentinel filtering, and Codex compatibility.
- Live smoke on 2026-09-11: OpenCode CLI `1.18.30`, `--model opencode/mimo-v2.5-free`, read-only task; parent `status=success` with a schema-valid report and mapped usage; no files created or changed.

Not implemented: `TEMOTE_OPENCODE_BIN` and persistent server/session/resume behavior.

## Phase 2 adapter extraction status (2026-09-11)

Backend adapter extraction landed on main without changing external behavior:

- `src/codex_delegation.rs` was split into `src/delegation/{mod,codex,opencode}.rs`; the delegation module path and all caller-facing functions are unchanged.
- Shared layer (`mod.rs`) keeps `DelegationBackend`, options/result/evidence/report types, CLI parsing and backend selection, artifact creation and permissions, bounded artifact capture, the shared process wait/capture helper, report validation, and parent serialization.
- `codex.rs` owns the Codex adapter: `build_codex_command` (including `--output-schema`), the Codex environment allowlist, Codex JSONL evidence/usage extraction, and Codex option validation.
- `opencode.rs` owns the OpenCode adapter: `opencode run` argv, `--pure`/`--format json`/`--dir`/`--model`/`--variant`, event/report extraction, session/usage mapping, option validation, and the read-only diagnostics probes (`--version`, `models --pure`) with their parsing/classification.
- Environment filtering and the two wait/capture paths are shared helpers with backend-specific allowlists/labels; no generic "arbitrary executable + argv" helper is exposed.
- All 37 delegation tests moved with their modules and pass unchanged; the frozen parent JSON fixture, Codex argv/environment tests, OpenCode argv/normalization tests, and diagnostics tests keep their assertions.
- Smoke on 2026-09-11: `delegate diagnose --backend opencode` unchanged; one-shot `delegate --backend opencode --model opencode/mimo-v2.5-free` completed with `status=success` and no files created or changed.

Remaining work after this slice: persistent server/session/resume.

## Phase 3 diagnostics status (2026-09-11)

Read-only OpenCode CLI diagnostics landed on main:

- `temote-mcp delegate diagnose --backend opencode [--model <provider/model>]` prints one bounded JSON document with `executable` (`available`/`unavailable`, `resolved`), `version` (`ready`/`unavailable`/`failed`/`timeout` plus a bounded value), `models` (`ready`/`unavailable`/`unsupported`/`failed`/`timeout` plus a bounded count and truncation flag), and `requested_model` (`present`/`absent`/`unknown`/`not_checked`).
- The backend selector honors explicit `--backend`, then `TEMOTE_DELEGATION_BACKEND`. Diagnostics currently implement only OpenCode and fail closed for Codex instead of reporting a false ready state.
- Probes are `opencode --version` and `opencode models --pure`, run through the existing bounded artifact/timeout subprocess helper with the same OpenCode child-environment allowlist. There are no login, auth, credential, config, model-download, session, or delegation side effects; child stdout/stderr is never echoed and only bounded identifiers/version values are reported.
- `opencode models` line output is treated as stable structured CLI output: lines are recognized only when they look like `provider/model` identifiers. Empty, failed, timed-out, or truncated listings map `requested_model` to `unknown` rather than reporting a false `absent`.
- Deterministic fake-CLI tests cover missing binary, version success/failure/timeout/oversized/malformed, model listing success/empty/mixed/failure/unsupported/timeout/oversized, requested-model present/absent/unknown, environment allowlist filtering, and secret/output non-leakage.
- Read-only smoke on 2026-09-11 with OpenCode CLI `1.18.30`: `executable=available`, `version=1.18.30`, `models=ready count=64`, `requested_model=opencode-go/deepseek-v4-flash present`; no credential mutation and no delegation execution.

Remaining work after this slice: persistent server/session/resume.

## Live comparative measurement status (2026-09-12)

Live comparison landed on main. Evidence: [`docs/evaluations/codex-vs-opencode-live-20260912.md`](../../docs/evaluations/codex-vs-opencode-live-20260912.md).

- 3 read-only tasks (repository comprehension, targeted review, implementation planning) × Codex/OpenCode × 3 runs = 18 delegation runs at baseline `0f8a795`, with fixed prompts: Codex `gpt-5.6-luna` (`high`) and OpenCode `opencode-go/deepseek-v4-flash` (`1.18.30`).
- Delivered normalized results: Codex 9/9; OpenCode 1/9 (`invalid_json` ×2 from raw newlines in strings, `invalid_report_schema` ×6 from summaries over the 1200-character bound). Blind content scores were close; the practical difference is report deliverability.
- Median wall-clock: Codex 176.3 s vs OpenCode 87.3 s across all tasks, with high variance in Codex's review task (180–446 s). Usage units are not comparable between backends; no cost conclusion.
- Recommendation in this sample: keep Codex as the default delegation backend; treat OpenCode as an interactive/session backend or a fallback only after report delivery is enforced. More evidence (more runs, second OpenCode model, report-contract fix) is needed before changing defaults.
- Observed issues are recorded in the evidence file only; no production changes were made in this slice.

Remaining work after this slice: persistent server/session/resume.

## OpenCode normalized-report delivery fix status (2026-09-12)

Follow-up fix landed on main. Report: [`docs/evaluations/opencode-normalized-report-fix-20260912.md`](../../docs/evaluations/opencode-normalized-report-fix-20260912.md); before/after detail appended to [`docs/evaluations/codex-vs-opencode-live-20260912.md`](../../docs/evaluations/codex-vs-opencode-live-20260912.md).

- Root cause: report delivery depended on the model emitting strict JSON within every schema bound; the adapter had no bounded repair or normalization. Raw newlines in strings caused `invalid_json`; summaries of 1380–2989 chars caused `invalid_report_schema`; requested values were read back from model output (double-quoted); `artifacts.report` was never written; per-step `step_finish` usage was dropped except for the last step.
- Fix: bounded report extraction (balanced-object scan with raw control-character sanitization), adapter-side normalization (canonical requested values, UTF-8-safe summary truncation with a ` …[truncated]` marker, bounded arrays/scalars), canonical report persisted to `artifacts.report` through one normalization path, accumulated per-step usage, and a valid prompt-contract example. The Codex adapter is unchanged.
- Verification: 19 new deterministic tests (44 OpenCode adapter tests), delegation 52, Codex 112, full cargo test 607 bin + 40 lib, gateway 60/60, fmt/clippy/check/diff green.
- Post-fix live recheck with the same frozen prompts: OpenCode normalized success went from 1/9 to 9/9 (`invalid_json` 2→0, `invalid_report_schema` 6→0). Remaining limitation: the 1200-char summary bound truncates long answers (marker visible), and the model rarely uses `checks`/`unresolved` for detail.

Remaining work after this slice: persistent server/session/resume.

## OpenCode executable override status (2026-09-12)

`TEMOTE_OPENCODE_BIN` implemented and verified locally. Report: [`docs/evaluations/opencode-bin-override-20260912.md`](../../docs/evaluations/opencode-bin-override-20260912.md).

- Contract: optional absolute path to an existing executable regular file; symlinks are canonicalized; takes precedence over PATH. An explicit but invalid override (empty, relative, missing, not a regular file, not executable, invalid value, or over the path-length bound) fails closed with a bounded error and does not fall back to PATH.
- One resolver (`resolve_opencode_executable`) is shared by delegation (`delegate --backend opencode`) and diagnostics (`delegate diagnose --backend opencode`). Delegation resolves at argument parsing before any child or artifact is created; diagnostics reports `available`/`unavailable`, the source (`env_override`/`path`/`invalid_override`), and a bounded reason for invalid overrides.
- The resolved physical path is never printed in diagnostics, results, or errors; the environment value is read only by the parent and is not passed to the OpenCode child. Codex delegation and the legacy `codex delegate` path are unaffected even when the override is invalid.
- Verification: 8 new adapter tests + 3 shared delegation tests (52 OpenCode adapter tests), including precedence, unset fallback, empty/relative/missing/directory/non-executable/NUL/overlong rejection, symlink canonicalization, invalid-override diagnostics, source labeling, error-path non-disclosure, and child-environment filtering.
- Smoke: valid override diagnosed `source=env_override`, `status=available`, `version=1.18.30`, requested model `present`; unset override diagnosed `source=path`; invalid override diagnosed `source=invalid_override` with `not_absolute`/`not_found` and delegation exited non-zero without producing a result; one-shot read-only delegation through the override returned `status=success` and left the disposable directory unchanged.

Remaining work after this slice: persistent server/session/resume.

## Phase 1 status (2026-09-11)

Phase 1 (freeze Codex behavior + boundary inventory + backend-neutral internal types) landed on main without launching OpenCode and without changing CLI output:

- `src/delegation/mod.rs` now exposes `DelegationBackend::{Codex}` with explicit `parse`/`name`, plus internal `NormalizedResult`/`NormalizedEvidence` types. `result_to_json` converts through `DelegationResult::normalize()` and `normalized_to_json()`.
- Compatibility is frozen by `parent_result_json_shape_is_frozen_for_compatibility` (exact parent JSON fixture), `normalized_result_keeps_requested_and_observed_distinct`, and the existing command/environment/report classification tests.
- No OpenCode process is launched; the generic CLI, `--format json` parsing, diagnostics, and binary override remain Phase 3.

### Boundary inventory: `src/local_agent.rs`

Reusable as implementation detail (copy or extract, do not import local-agent authorization):

- `Agent::parse`/`as_str`/`executable_name` naming conventions and the OpenCode binary name `opencode`.
- Command-shape knowledge recorded by `build_opencode_command` (`run`, `--pure`, `--format json`, `--dir`, optional `--model`, task delivery), subject to re-verification against a pinned delegation CLI version.
- `opencode_config` permission JSON shape as an input to a delegation-specific config if one is required.

Local-agent-only policy; must not become the delegation contract:

- `resolve_executable_details`/`resolve_explicit_executable` enforce session-root exclusion and are coupled to local-agent executable resolution; delegation needs its own executable policy and diagnostics.
- `AgentState` auth import, HOME/XDG isolation, and `--pure`/config injection are local-agent sandbox behavior.
- `prepare`/`run`/`revalidate` approvals, job ownership, and `LocalAgentScope` sandbox semantics stay in `local_agent`.
- There is no existing version-probing helper; delegation diagnostics must add one for the pinned OpenCode version.
- `local_agent_run` fixtures are local-agent-shaped; delegation fixtures stay separate unless the exact pinned output is identical.
