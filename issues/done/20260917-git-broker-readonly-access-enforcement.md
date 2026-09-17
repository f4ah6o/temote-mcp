# Git broker must enforce per-run `access` on mutation requests

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: none

## Current code and contract

`local_agent::run` starts `GitBroker` for every `local_agent_run` (`src/local_agent.rs:758`)
regardless of `PreparedRun.access`, and `GitBroker::start` receives only the session and session
roots (`src/agent_git.rs:258`). The broker does not receive `Access`; `handle_request` will execute
`switch`/`add`/`commit` for a `read_only` run as long as the child writes a request. The child's
own agent permission config is a separate layer and must not be the only enforcement point.

## Reproduction

- Build a `PreparedRun` with `Access::ReadOnly`, call `run`, and have the fake agent write a
  `switch` request into `$TEMOTE_MCP_GIT_BROKER_DIR/requests/`; the broker currently mutates the
  fixture repository.
- Unit level: `handle_request` has no access parameter, so a read-only caller cannot be rejected.

## The one responsibility to change

Pass the per-run `Access` into the broker and enforce it on the parent side before any Git process
starts:

- `GitBroker::start(..., access: Access)` stores it in `BrokerState`.
- `handle_request` rejects every mutation (`switch`, `switch -c`, `add`, `commit`) with the fixed
  rejection payload when `access == Access::ReadOnly`, before classification-side effects.
- `local_agent::run` passes `prepared.access`; `Access::WorkspaceWrite` keeps today's behavior.
- `--yolo` sessions do not bypass `access`; it is a per-run bound.

## Not changing

- No widening of the sandbox or `.git` write access; no change to structured Git tools.
- Agent permission config (`codex_access_contract`, `opencode_config`) remains defense in depth.
- Read-only Git commands are Packet F; this packet only rejects the existing mutation set.

## Focused tests

- `agent_git` unit: read-only `handle_request` for `switch`, `switch -c`, `add`, `commit` returns
  the fixed rejection; HEAD, branch, index and working tree are unchanged afterwards.
- `local_agent` wiring: real `run()` + fake agent that writes broker requests, for both
  `ReadOnly` (rejected, repo unchanged) and `WorkspaceWrite` (positive control succeeds).

## Host / CI / provider verification

- Linux host/CI: the wiring test shells out to host `git` inside the existing local-agent sandbox.
- macOS host execution: NOT RUN unless a macOS runner is used.

## Completion condition

Read-only runs cannot mutate through the broker; workspace-write runs still can; focused tests and
`just sandboxed-check` pass.

## Implementation notes (2026-09-17)

Changes:

- `src/agent_git.rs`: `GitBroker::start(directory, session, workspace, access)` stores the per-run
  `Access` in `BrokerState`; `handle_request` rejects every mutation with the fixed error payload
  when `access != Access::WorkspaceWrite`, before classification or any Git process.
- `src/local_agent.rs::run` passes `prepared.access` and `prepared.cwd` to the broker.

Focused tests:

- `cargo test --bin temote-mcp --locked agent_git::tests::broker_rejects_mutations_for_read_only_access_and_preserves_the_repository`:
  PASS — `switch`/`switch -c`/`add`/`commit` all rejected; HEAD, branch, `.git/index` bytes and
  working tree unchanged.
- `cargo test --bin temote-mcp --locked local_agent::tests::local_agent_broker_enforces_the_per_run_access_through_the_real_wiring`:
  PASS — real `run()` + fake agent writing a broker request through the private queue; read-only
  run is rejected and creates no branch, workspace-write run stages the named file. Repeated 3/3.
- `cargo test --bin temote-mcp --all-features --locked agent_git`: 26/26 PASS, 10 consecutive runs.

NOT RUN: macOS host execution, live Codex/OpenCode end-to-end.

## 2026-09-17 completion

Repository-local acceptance is met and the repair round above passed independent review; the
remaining NOT RUN rows above stay host/CI or live-matrix gates.
