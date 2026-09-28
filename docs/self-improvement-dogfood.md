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

Fixture runs cover all six checked-in logical scenarios without a provider. A
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
The runner does not create or stop sessions. `--max-polls` (default 20) and
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
