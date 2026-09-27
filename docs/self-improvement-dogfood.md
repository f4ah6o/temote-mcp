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
  --model <available-model> --effort <available-effort>
```

The live task asks the backend for a short read-only repository status report.
`--terminal-read-strategy reread` provides a reproducible baseline for the
older client behavior; the default `reuse` consumes the terminal poll's
evidence reference directly. Both strategies use the same scenario revision.
The runner does not create or stop sessions. `--max-polls` (default 20) and
`--poll-interval` (default 1 second) bound waiting. A run reaching the limit is
`blocked`, never a pass. Runs go to ignored, owner-local `dogfood/runs/` unless
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
Qualification requires a measured target improvement, passing candidate
assertions, no assertion regression, and explicit passing gates. The comparison
keeps source event references for every numeric value; it is a vector, not a
single friction score. `qualified` means the candidate met these acceptance
conditions. It does not publish a release. Release qualification uses the
repository's normal checks and existing CalVer workflow.
The `release-qualification` scenario additionally requires separate
`final_diff`, `tests`, `git_status`, `ci`, and `action_result` gate results.
Missing or `not_run` CI/action evidence blocks that scenario; a local
self-improvement comparison does not invoke the release workflow.

The fixture fault cases model an uncertain accepted start and a transient poll
error. They test idempotent replay and safe recovery, while provider-specific
live behavior remains a separate host gate. `blocked` and `not_run` are never
counted as CI success.
