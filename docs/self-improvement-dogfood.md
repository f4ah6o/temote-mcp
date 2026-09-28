# Self-improvement dogfood harness

`dogfood/` is the repository-owned scenario and observation harness for the
[self-improvement protocol](../issues/done/20260928-self-improvement-dogfood-protocol.md).
It uses Python 3's standard library. It does not add an MCP tool or a release path.

## Run

```sh
python3 -m dogfood validate
python3 -m unittest dogfood.test_protocol
python3 -m dogfood run baseline delegation-lifecycle --poll-interval 0
```

`python3 -m dogfood validate` validates every checked-in logical scenario without a provider. A
live run uses an **existing active local session** and a specified Temote binary:

```sh
python3 -m dogfood run baseline delegation-lifecycle \
  --adapter live --binary target/debug/temote-mcp \
  --session-id <existing-session-id> --backend codex \
  --model <available-model> --effort <available-effort> \
  --max-polls 200 --poll-interval 1
```

The live task asks the backend for a short read-only repository status report.
`--terminal-read-strategy reread` provides a reproducible baseline for the
older client behavior; the default `reuse` consumes the terminal poll's
evidence reference directly. Both strategies use the same scenario revision.
The delegation-lifecycle runner does not create or stop sessions; the repository-setup scenario creates its development session as described below. `--max-polls` (default 20) and
`--poll-interval` (default 1 second) bound waiting. A run reaching the limit is
`blocked`, never a pass. The live example uses a longer bounded wait because
reasoning tasks can exceed the default 20 polls; choose the same waiting policy
for baseline and candidate. Runs go to ignored, owner-local `dogfood/runs/` unless
`--output` selects another path. Existing artifacts are never overwritten.
The live adapter keeps the stdio MCP process alive while evidence is read. For
rediscovery it opens a second MCP process while the runtime owner remains alive;
after terminal state, it closes the old owner if a new process needs to mint
terminal evidence. That extra recovery call is observed.

Each run records scenario revision, repository HEAD, invoked server binary
SHA-256, public contract fingerprint, backend/model selectors, assertions,
bounded call events, and derived metrics. The repository HEAD and running
binary identity are separate fields. A dirty checkout is not represented by
HEAD alone; retain the exact diff or commit the candidate before treating a
run as release evidence. The tool request and response bodies are never saved:
events contain argument **names**, state, stable error code, byte count, and
next-action classification. Child output and evidence content stay in memory.
Run files are capped at 1 MiB and created with mode `0600`.

Compare the same scenario revision with independently observed gate results:

```sh
python3 -m dogfood compare baseline.json candidate.json \
  --target-operation read_terminal_result --gates gates.json --output comparison.json
```

`gates.json` maps gate names to `pass`, `fail`, `blocked`, or `not_run`. An
assertion fixed by the candidate can be targeted with `--target-assertion`.
Scalar metrics can be targeted with `--target-metric`; operation call counts
use `--target-operation`. Per-operation goals avoid mistaking variation in
poll timing for a regression or an improvement in the targeted step.
Qualification requires passing candidate assertions, no assertion or selected
metric regression, comparable backend/model/effort selectors, and explicit
passing gates. A measured improvement is optional. The comparison
keeps source event references for every numeric value; it is a vector, not a
single friction score. `qualified` means the candidate met these acceptance
conditions. It does not publish a release. Release qualification uses the
repository's normal checks and existing CalVer workflow.

## Issue implementation and improvement assessment

Implementing a Temote MCP issue is itself a dogfood activity. If the issue's
acceptance criteria and relevant checks pass, and the dogfood scenario exposes
no regression, completing the issue without Temote-specific friction is a
successful result. It does not need to improve delegation metrics.

Record independently verified issue completion and checks as gates, for example:

```json
{"issue_completion": "pass", "tests": "pass", "final_diff": "pass"}
```

`issue_completion` must reflect the implemented issue's acceptance criteria.
The read-only delegated status task used by the current live adapter does not
implement an issue or establish that gate on its own. Preserve the issue's
implementation and verification evidence separately.

The comparison reports two independent fields:

| `improvement` | Meaning | `qualification` when all acceptance checks pass |
| --- | --- | --- |
| `improved` | At least one selected metric decreases or a targeted assertion is repaired, with no regression. | `qualified` |
| `unchanged` | No measured improvement or regression in the selected targets; also used when no improvement target was selected. | `qualified` |
| `regressed` | A selected metric increases or a previously passing assertion stops passing. | `blocked` |
| `not_evaluated` | Selectors differ or the candidate did not provide usable passing observations. | `blocked` |

With no improvement target, `unchanged` means that no improvement was assessed;
it does not claim that every recorded metric is numerically identical. Polling
variation remains visible in the report but is not a regression gate unless
explicitly selected. When several targets are selected, any worsening blocks
qualification even if another target improves.

Missing, failed, `blocked`, or `not_run` acceptance gates also produce
`qualification: blocked`, independently of `improvement`. Thus an observed
improvement cannot compensate for an incomplete issue or failing checks.
An accepted unchanged comparison exits with status 0, just like an improvement.
Blocked comparisons exit with status 1. Old comparison files are immutable;
rerun `compare` into a new output file to use these semantics.

## Memory continuity qualification

The `memory-continuity` scenario (revision 1) checks the Fabric observation and
knowledge path with two ordinary implementation tasks. Task A requires JSON
output and leaves a field-set question unresolved. A second head resolves the
repository context, then task B explicitly changes the output policy to TOML.
Neither task asks the coding agent to save or summarize memory. Its independent
assertions cover supported knowledge and support references, unresolved
context, cross-head repository lookup, explicit supersession of the JSON
constraint, Queue replay deduplication, and isolation from an unrelated
repository.

The Node harness has two fixture modes. `--mode fixture` runs the deterministic
Miniflare/D1/Queue integration test with synthetic observations. In live mode,
`--extractor fixture` exercises dedicated Temote hosts without calling a real
model. Neither mode is live synthesis qualification. From the repository root,
run:

```sh
node gateway/scripts/memory-dogfood.mjs --phase baseline --mode fixture --extractor disabled
node gateway/scripts/memory-dogfood.mjs --phase candidate --mode fixture --extractor fixture
```

A fixture run reports `live_synthesis: NOT RUN` and `qualification:
not_qualified`. It verifies the local D1/Queue worker integration only; it is
not evidence that a real model produced knowledge.

Live mode starts two dedicated local supervisors and host agents with isolated
`XDG_STATE_HOME` directories and temporary credentials. The candidate run
waits for host A's observation to sync, stops only that harness-owned host,
resolves repository context while it is offline, runs task B on host B, and
checks Queue replay and repository isolation. It never stops a production host
or deploys to Cloudflare. `npm ci` in `gateway/` is required first. Preserve
the exact baseline binary and use the candidate binary built from the tested
checkout. The task model and `--effort` select the delegated coding run; the
extractor model and `--extractor-reasoning-effort` independently configure
knowledge synthesis. Use the same task backend, model, effort, and extractor
profile for both phases. The extractor reasoning option is candidate-only,
requires `--extractor live`, accepts `low`, `medium`, `high`, `minimal`, `none`,
`max`, or `xhigh`, and is omitted from the provider request when unset or blank.
The environment fallback is `TEMOTE_MCP_MEMORY_REASONING_EFFORT`.

```sh
node gateway/scripts/memory-dogfood.mjs --phase baseline --mode live \
  --extractor disabled \
  --baseline-binary dogfood/runs/memory-20260928/baseline-bin/temote-mcp \
  --backend codex --model <available-model> --effort <available-effort> \
  --extractor-profile opencode-go/glm-5.3-flash

node gateway/scripts/memory-dogfood.mjs --phase candidate --mode live \
  --extractor live --candidate-binary target/debug/temote-mcp \
  --backend codex --model <same-model> --effort <same-effort> \
  --extractor-profile opencode-go/glm-5.3-flash \
  --extractor-endpoint https://opencode.ai/zen/go/v1/chat/completions \
  --extractor-model glm-5.3-flash \
  --extractor-reasoning-effort low \
  --host-wait-seconds 60 --pipeline-wait-seconds 240 --task-polls 200
```

The live extractor key is read from the local OpenCode auth profile (default
`~/.local/share/opencode/auth.json`; use `--auth-file` for another path). Never
put the key in command arguments, environment dumps, or shared artifacts. A
candidate run with `--extractor fixture` covers host synchronization and
context continuity but reports live synthesis as `NOT RUN`. The harness prints
the scenario artifact path and gate states; keep its run directory under
ignored `dogfood/runs/`.

Compare the baseline and candidate scenario artifacts with the independent
qualification gates. The report remains blocked until every required gate is
present and passing:

```sh
python3 -m dogfood compare <baseline-scenario.json> <candidate-scenario.json> \
  --gates <owner-local-independent-gates.json> \
  --output dogfood/runs/memory-continuity-comparison.json
```

Missing credentials, an unavailable model, failed assertions, or unperformed
gates remain `blocked` or `not_run`; a successful fixture run cannot substitute
for them. Run artifacts contain bounded event metadata and gate results, not
task text, model input/output, credentials, or evidence bodies. Cloudflare
deployment and remote E2E remain separate qualification gates.

## Fabric metadata and deployment loop

The delegated task scenario observes execution behavior; it does not discover
new tool definitions or deploy Fabric. Use the runtime observations to identify
friction, then change the authoritative Rust registry when a tool needs a fix.
Fabric (currently `gateway/`) consumes the generated public host-routing
projection, including descriptions and input schemas:

```text
src/mcp.rs::tools() + public host-routing projection
  -> just generate-tools
  -> gateway/contract/routed-tool-metadata.json
  -> gateway/src/protocol.js::PUBLIC_TOOLS
  -> Worker MCP tools/list
```

After retaining a baseline binary and observation, run:

```sh
just generate-tools
just check-generated
npm ci --prefix gateway
npm test --prefix gateway
npm run deploy:dry-run --prefix gateway
```

Run generation twice and compare the artifact bytes. The generator’s fixture
tests additionally cover add/remove/rename, prose changes, malformed metadata,
and stale outputs. These checks need no live provider. Build the candidate and
repeat the live scenario using the baseline backend/model/effort. Retain the
candidate diff under the ignored run directory because HEAD alone does not
identify uncommitted changes.

Follow [Fabric deployment](gateway.md#deploy) to configure and inspect the
existing public target, Access, D1 and secrets before deploying. Verify the
Worker health fingerprint and authenticated `tools/list` against the generated
metadata, including descriptions, schemas, count and representative tools.
Record build, generated-state, runtime and deployment outcomes as independent
gates. An unavailable credential, failed live task, or missing endpoint check
is `blocked`/`not_run`; fixture success never substitutes for it. Local
implementation qualification and Cloudflare deployment qualification may use
separate gate maps so a local passing comparison does not claim remote success.

The [2026-09-28 Fabric dogfood evaluation](evaluations/fabric-dogfood-20260928.md)
records the local cycle, the initially blocked Cloudflare gates, subsequent
authenticated deployment and plugin verification, and persistent host startup.

## Release qualification

The `release-qualification` scenario additionally requires separate
`final_diff`, `tests`, `git_status`, `ci`, and `action_result` gate results.
Missing or `not_run` CI/action evidence blocks that scenario; a local
self-improvement comparison does not invoke the release workflow.

The fixture fault cases model an uncertain accepted start and a transient poll
error. They test idempotent replay and safe recovery, while provider-specific
live behavior remains a separate host gate. `blocked` and `not_run` are never
counted as CI success.

## Develop with jj

In an existing clean Git checkout, initialize a colocated jj workspace once.
Colocation preserves the Git metadata expected by the harness:

```sh
jj git init --colocate
jj status
jj describe -m "feat: describe the candidate improvement"
jj bookmark create codex/dogfood-candidate -r @
```

Use `jj diff` to review the candidate and `jj op log` to inspect local operation
history. The harness records Git `HEAD`, which can identify the parent of jj's
working-copy commit rather than the candidate itself. Retain `jj log -r @`,
`jj diff --git`, and `jj op log --limit 5` alongside the run artifacts so that
the exact tested change remains identifiable. Run artifacts belong under the
ignored `dogfood/runs/` directory.

Build the baseline **before editing code** with `cargo build --bins --locked`.
When preserving a baseline binary in another directory on Linux, also preserve
the sibling `temote-linux-sandbox` helper; both live delegation and `doctor`
depend on that helper. After editing, rebuild the candidate and rerun the same
scenario with the same backend/model/effort selectors. Run `temote-mcp doctor`
for both binaries. The candidate's `development jj` check tests `jj --version`;
it does not initialize or snapshot the repository.

The current logical scenarios measure delegated task behavior. A new CLI
diagnostic should additionally retain its own before/after output and gate
result. Do not attribute polling or result-read variation to a doctor change.
A passing self-host scenario alone does not prove an issue is complete. With
independent passing issue-completion and verification gates, `compare` can
report `qualified` and `unchanged`; the new diagnostic's benefit remains
supported by its own before/after evidence.

## Bare repository setup

`repository-setup` revision 1 measures root preparation → bare clone → exact
operation replay after reconnect → linked worktree → normal session → a jj
change. Add a new logical scenario before implementing a flow that the existing
scenarios cannot measure. The fixture proves the harness contract only.

```sh
python3 -m dogfood run baseline repository-setup --adapter live \
  --binary <preserved-baseline-binary> --session-id <existing-root-session> \
  --root <named-root> --source <named-root>/<repository> \
  --model <available-model> --effort <available-effort> \
  --lifecycle-url https://<host>/mcp --lifecycle-token-env <token-variable> \
  --max-polls 200 --poll-interval 1 --output <baseline.json>
```

Use the same options with `candidate` and the rebuilt candidate binary. A
unique destination is generated unless `--destination` supplies one. Clone
sources can also be HTTPS URLs admitted by the product; compare equivalent
source kinds and network/auth conditions. Clone, worktree, verification, and jj
filesystem operations run in delegated agents. The harness supplies logical
paths and retains only argument names and bounded observations. The final
verification reads only the generated bare repository and worktree; success
must appear in the final assistant report, never merely in its input prompt.

Delegation uses stdio MCP; worktree `session_start` uses the existing authenticated
HTTP MCP endpoint because lifecycle creation is intentionally unavailable on
stdio. Supply an existing OAuth bearer token through the named environment
variable, never an argument or artifact. The HTTP adapter refuses redirects,
credential-bearing URLs, and unencrypted endpoints except loopback fixtures.
The endpoint must use Temote local OAuth; Cloudflare Access clients need their
own assertion-header adapter. Keep lifecycle transport equivalent in the comparison.

After creating the normal worktree session, the runner selects it through a
fresh stdio MCP transport and observes `session_info` again. Count this extra
selection step and call; keep the preparation owner alive until its tasks are
terminal. Earlier mixed-session transport observations remain separate failures,
and transport separation does not establish their underlying cause.

The existing root session is never stopped or restarted. A successful run
leaves its new worktree session active and the repository intact for inspection.
A failed or blocked run may leave an accepted task or partial repository: use
`task_list`, `codex_task_get`, and scoped evidence to reconcile it before another
attempt. Never replace an uncertain clone's `operation_id` merely to retry.
Exact-replay recovery and admission probes are measured calls. Initialization,
building the binaries, `doctor`, and human setup/approval steps are separate
prerequisites and must be reported alongside the run. A missing baseline tool
is a failure with downstream steps `not_run`, never a zero-call success.

Compare `--target-assertion bare_clone_completed` with independently observed
admission, repository-completion, verification, and normal development gates.
A candidate blocked by an older supervisor or unavailable child approval is
recorded as blocked and cannot qualify. Preserve an exact `jj diff --git`, jj
change/commit and operation identities, and both binary/helper SHA-256 identities
next to the runs. `doctor` jj diagnostics are a prerequisite, evaluated
separately from the repository-setup feature.

On the measured jj 0.45.1, `jj git init --colocate` refuses a linked Git
worktree. A backing repository outside the worktree would require writes
outside the normal worktree session's scope. The scenario therefore imports
`HEAD` into a separate jj backing repository **inside** that worktree:

```sh
git init --bare .jj-backing.git
git --git-dir .jj-backing.git fetch ../.. HEAD:refs/heads/seed
git --git-dir .jj-backing.git symbolic-ref HEAD refs/heads/seed
jj --config 'snapshot.auto-track="none()"' git init --git-repo .jj-backing.git
# Create the development file, then explicitly track it:
jj --config 'snapshot.auto-track="none()"' file track DOGFOOD_JJ.txt
jj --config 'snapshot.auto-track="none()"' describe -m 'test: prove repository setup through jj'
jj --config 'snapshot.auto-track="none()"' status
jj --config 'snapshot.auto-track="none()"' diff --git
```

`../..` assumes the scenario's `<bare>/.wt/development` layout. Local fetching
reads the source bare repository; all new jj metadata stays in the worktree.
The backing directory remains untracked. These are four preparation commands
plus explicit file tracking and the repeated CLI config argument, and must be
reported as additional steps. Do not persist this setting with `jj config set
--repo`: jj 0.45.1's secure repo configuration needs a writable host config
directory outside the worktree. The CLI-only form was measured with every
other host path read-only and avoids generating a repo `config-id`. Verification
uses `--ignore-working-copy` with the same CLI config and does not repair or
migrate configuration. See [jj secure config](https://docs.jj-vcs.dev/latest/design/secure-config/).
Changes in this independent jj backing repository are not automatically
exported to the original bare repository. The main development checkout can
continue to use its existing colocated jj workspace.

The [2026-09-28 bare-clone report](dogfood-bare-clone-20260928.md) preserves
measured failures, recovery calls, independent gates and blocked comparisons.
