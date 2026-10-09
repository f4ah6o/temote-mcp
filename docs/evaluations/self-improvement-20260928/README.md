# Self-improvement dogfood cycle — 2026-09-28

This is one live, read-only Temote MCP cycle for the repository-owned
[`delegation-lifecycle`](../../../dogfood/scenarios/delegation-lifecycle.json)
scenario. Both phases ran from repository HEAD `26ac102` with the same local
Temote MCP binary SHA-256
`446f6ef40871d17be94c3fb9c55ef0ab3d47ec2e30ebdc41473f2fa445e30764`
and public contract fingerprint
`ef6818b96867949403ec51ed29b92f10a9256d6172c9601215ac090607fdc78e`.
The selected backend was Codex with `gpt-6-luna` at `max` effort. The existing
active `temote-self-dogfood-20260927` session was used; the harness did not
stop or restart it. `repository_head` and invoked server binary identity are
recorded separately in each run.

The observed friction was an avoidable terminal `codex_task_get`: the older
client strategy re-read the task after its final poll, before reading evidence.
The candidate strategy reuses the final poll's evidence reference. The
implementation is the default `reuse` strategy in `dogfood/runner.py` and the
instruction in `skills/temote-mcp/SKILL.md` (commit `b29fef4`). Both strategies
remain selectable in the committed harness for reproduction.

| Observation | Baseline (`reread`) | Candidate (`reuse`) |
| --- | ---: | ---: |
| `read_terminal_result` calls | 2 | 1 |
| Total MCP calls | 16 | 13 |
| Poll calls | 12 | 10 |
| Duplicate starts | 0 | 0 |
| Ambiguous terminal states | 0 | 0 |

The per-operation reduction is the acceptance target. Total calls and poll
calls include provider timing variation, so the 16-to-13 total is an observed
sample, not a guaranteed three-call saving. Numeric values and event refs are
in [`comparison.json`](comparison.json); source events are in
[`baseline.json`](baseline.json) and [`candidate.json`](candidate.json).
The evaluator returned `qualified` for this *client workflow* improvement.
All candidate assertions passed, and the explicit local checks in
[`gates.json`](gates.json) passed. This is not a Temote server behavior change
or a release decision. CI and release action results are not included.

The same committed harness also ran these live candidate scenarios:

| Scenario | Result | MCP calls | Evidence |
| --- | --- | ---: | --- |
| Task rediscovery across two stdio processes | pass | 15 | [`task-rediscovery.json`](task-rediscovery.json) |
| Injected transient poll error, then safe `task_get` | pass | 17 | [`transient-poll-failure.json`](transient-poll-failure.json) |
| Uncertain start response, exact `operation_id` retry | pass | 14 | [`duplicate-start.json`](duplicate-start.json) |
| Repository/server identity recorded separately | pass | 15 | [`self-host.json`](self-host.json) |
| Read-only release-qualification composition | blocked | 10 | [`release-qualification.json`](release-qualification.json) |

Release qualification is `blocked` because `final_diff`, `git_status`, `ci`,
and `action_result` were `not_run` in
[`release-gates.json`](release-gates.json). This does not block the scoped
client-workflow comparison above; it prevents treating local checks as a
release decision.

The transient error is injected at the harness transport boundary, not a
provider outage. The uncertain start drops an accepted response and repeats
the exact request; no duplicate task was observed. Rediscovery keeps the
original MCP owner process alive while a second process locates and polls the
task. The terminal evidence reference is unavailable to the second process
while the first holds the runtime lease, so the harness closes the first
process after terminal state and makes one recovery `task_get` call. This is
measured friction, not a claim that arbitrary MCP process loss preserves a
running child. The separate build-identity fields were verified; launching a
different candidate Temote server build was not part of this workflow change.

Follow-up: evaluate a server-side terminal evidence handoff across live MCP
processes, without weakening the runtime lease or bounded evidence boundary.
The existing [bounded wait issue](../../../issues/doing/20260927-bounded-wait-for-delegated-tasks.md)
still owns the repeated poll overhead. Neither follow-up is counted as fixed
by this cycle.
