# Git broker response authority: agent must not be able to author broker responses

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: `issues/polished/20260917-git-broker-atomic-response-outcome.md`

## Current code and contract

`local_agent::run` makes the whole `AgentState.root` writable in the agent sandbox
(`src/local_agent.rs`), and the Git broker queue lives at
`<state_root>/git-broker/{requests,responses}` (`AgentState::broker_directory`). The shim reads
`responses/<id>.json` and decodes it as the authoritative outcome
(`request_broker_with_timeout`, `src/agent_git.rs`). Therefore the agent can create a forged
`responses/<id>.json` after observing a valid request ID:

- `{"schema":1,"error":"..."}` -> shim reports `Rejected` while the broker may still apply the
  mutation;
- `{"schema":1,"status":0,"stdout":"","stderr":""}` -> shim reports `Completed` for an operation the
  broker rejected.

This breaks the Packet E contract that only the broker authors `Completed` / `Rejected`, and that
`Indeterminate` is the only honest answer when the broker did not answer.

## The one responsibility to change

Establish filesystem response authority:

- `requests/` stays agent/shim-writable (under the private state root);
- the responses queue moves outside every agent-writable root into its own private parent-owned
  directory, and is exposed to the sandbox only as a read-only visible root;
- only the parent broker publishes responses (it holds the directory and writes outside the
  sandbox); the shim only reads them;
- shim response cleanup must not require agent write access: responses are owned and cleaned up by
  the broker/parent lifetime, not by the shim;
- broker drop removes both queue roots (parent ownership unchanged);
- no shared secret in agent-readable requests and no reliance on filename unpredictability;
- the state root stays writable so Codex/OpenCode private state keeps working; the responses
  subtree is excluded narrowly, not the whole state root.

The same contract must be expressed for Linux (read-only visible root in the local-agent policy)
and macOS (visible read root plus the write-policy exclusion/absence).

## Not changing

- Request schema, mutation allowlist, `Access` enforcement, workspace scope, and outcome semantics
  (`Completed` / `Rejected` / `Indeterminate`) stay.
- A strict decode already rejects unknown-schema payloads; it remains defense in depth, not the
  authority mechanism.

## Adversarial tests (required)

A. forged `Completed`: fake agent enqueues a valid request, observes the ID, tries to write
   `responses/<id>.json` with a success payload; the sandboxed write fails and the shim only sees
   the broker's real response.
B. forged `Rejected`: same with an error payload; when the broker completes a mutation the shim
   does not report `Rejected`.
C. replace/unlink/rename: response entry and directory cannot be unlinked, replaced, or renamed
   from the sandbox; `requests/` enqueue still works.
D. positive controls: `workspace_write` switch/add/commit still complete, `read_only` mutations are
   still rejected, timeout is still `Indeterminate`.

## Host / CI / provider verification

- Linux host/CI: real local-agent sandbox wiring test plus `just linux-sandbox-acceptance`.
- macOS: no-default compile check locally; seatbelt execution is a CI/macOS gate (NOT RUN here).

## Completion condition

The sandboxed agent cannot author, replace, unlink, or rename any response, the broker's response is
the only one the shim can observe, and the positive controls plus `just sandboxed-check` pass.

## Implementation notes (2026-09-17)

Changes:

- `src/local_agent.rs`: `AgentState` gains a parent-owned `broker_responses_directory`
  (`<temp-base>/temote-mcp-git-responses-<uuid>`, 0700, outside the agent-writable state root),
  created before the sandbox starts, removed on `AgentState` drop, and exported as
  `TEMOTE_MCP_GIT_BROKER_RESPONSES_DIR`. `run` adds it to the sandbox's `read_only_roots`.
- `src/agent_git.rs`: `BrokerQueue` now takes two roots (`requests` under the state root,
  `responses` outside it); `GitBroker::start`/`Drop` own and remove both; the shim reads the
  response queue read-only and never deletes responses (broker/parent owns cleanup); mutation
  routing requires both queues.
- Linux: the responses root is a read-only visible bind under the hidden temp base. macOS:
  `SandboxSpec::local_agent` re-exposes it via `read_only_roots` and the write policy only allows
  `writable_roots`, so it cannot be written/renamed.

Measured adversarial results (Linux host, real local-agent sandbox):

- `local_agent::tests::local_agent_sandbox_cannot_forge_broker_responses` PASS (5/5 repeats):
  report lines inside the sandbox are `enqueue=allowed`, `forge_completed=denied`,
  `forge_rejected=denied`, `unlink_response=denied`, `rename_response=denied`,
  `rename_response_dir=denied`, `mkdir_response_dir=denied`; read-only run observes
  `outcome=rejected` and creates no branch; workspace-write run observes `outcome=completed` and
  stages the named file.
- `sandbox::linux_tests::linux_local_agent_git_shim_executes_from_state_and_uses_the_private_queue`
  PASS after updating it to the split roots: forgery report `denied`, unlink report `kept`, broker
  response readable, request still enqueued.
- `agent_git` 27/27 PASS 8 consecutive runs; `local_agent` wiring positive control PASS 3/3.
- macOS: `cargo check --no-default-features --target x86_64-apple-darwin --lib --tests` compiles
  the new `sandbox::macos` unit test with no errors in `macos.rs`/`policy.rs`; Seatbelt execution
  is a CI/macOS gate (NOT RUN locally).

Correction to Packet E/D notes: response cleanup is now broker/parent-owned; the shim does not
delete responses (that would need agent write access). Packet D's single-root queue description is
superseded by this two-root layout.

## Strict decode and trust model

- `decode_response` now uses typed, `deny_unknown_fields` shapes: `Rejected` requires exactly
  `{schema, error}` with the fixed rejection message; `Completed` requires exactly
  `{schema, status, stdout, stderr}` with the right types. Anything else is a protocol error
  (already mapped to `Indeterminate`), never `Rejected`.
- Trust model: the parent sets `TEMOTE_MCP_GIT_BROKER_DIR` and
  `TEMOTE_MCP_GIT_BROKER_RESPONSES_DIR` for the agent process. An agent that overrides its own
  environment and points its own shim at an attacker-authored queue is lying to itself; it cannot
  write into the parent-owned response root, cannot alter broker mutations, and cannot change the
  repository state that the parent observes. The filesystem authority, not a shared secret or a
  filename guess, is what makes broker outcomes authoritative.
