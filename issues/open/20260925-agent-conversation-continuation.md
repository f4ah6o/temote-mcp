# Codex app-server: related tasks should optionally continue an existing conversation thread

Status: open  
Created: 2026-09-25 (Asia/Tokyo)  
Repository: `f4ah6o/temote-mcp`  
Related:

- `issues/open/20260924-temote-development-harness-restructure.md`
- `issues/open/20260922-agent-server-backends-cli-deprecation.md`
- `src/codex_app_server.rs`

## 1. Problem

The current Codex app-server backend preserves and resumes a Codex thread for the **same Temote task**, but a newly-created Temote task always starts a new Codex thread.

Current behavior in `src/codex_app_server.rs`:

- task start calls `thread/start` and stores the returned `thread_id`
- a lost/expired in-memory runtime can be reconstructed by starting a new `codex app-server --stdio` process and calling `thread/resume` with the retained `thread_id`
- therefore the Codex app-server process lifetime is already separate from the Codex conversation-thread lifetime
- however a second, related Temote task has no contract for intentionally reusing the previous task's conversation context

This loses useful context for follow-up work such as:

- implement a feature -> fix the tests for the same feature
- investigate a CI failure -> apply the discovered fix
- implement a packet -> address review findings for that packet
- continue a multi-step task after the previous execution was intentionally closed

The desired behavior is **not** to merge Temote tasks together. Temote task identity, execution, verification, delivery, operation receipts, and ownership must remain distinct even when two tasks reuse the same agent conversation.

## 2. Goal

Allow a caller to explicitly start a new logical Temote task while continuing an eligible previous agent conversation.

Conceptually:

```text
task A
  task_id = A
  backend = codex
  backend conversation = thread X

task B
  task_id = B
  continue_from = A
        |
        +--> reuse thread X
             + new turn for task B
```

The new task remains independently observable and verifiable.

## 3. Fixed design direction

### 3.1 Dependency and conversation continuation are different concepts

Do not infer conversation reuse from task dependencies alone.

Keep these concepts independent:

```text
depends_on: <task id>       # orchestration / ordering relationship
continue_from: <task id>    # agent conversation-context relationship
```

A task may depend on another task without benefiting from the same conversation context.

### 3.2 Continuation must be explicit at first

The initial contract should require an explicit caller choice.

Suggested shape:

```text
continuation:
  new
  previous_task:<task_id>
```

Equivalent typed representations are fine, but do not use a free-form backend command or raw thread ID supplied by an untrusted caller.

Default behavior remains `new`.

Automatic semantic matching of "related" tasks is out of scope for the first slice.

### 3.3 Temote task identity must not become Codex thread identity

A continued Codex thread may contain turns belonging to multiple Temote tasks.

Therefore:

- a new `task_id` is still allocated
- a new task record and operation receipt are still created
- verification and delivery state remain task-specific
- evidence ownership must identify the Temote task that requested the new turn
- completing task A must not imply task B is completed
- task B must not inherit task A's verification PASS
- task B must not inherit task A's delivery state

### 3.4 Backend continuation is capability-driven

Codex can map continuation to retained `thread_id` + `thread/resume` / new `turn/start`.

Other backends may have different session/thread semantics.

The orchestration contract should expose continuation capability without pretending every backend implements it identically.

Examples of capability states:

- supported
- unsupported
- unavailable because the prior backend conversation is no longer resumable

Backend-specific native identifiers remain internal implementation details.

## 4. Codex implementation expectations

For `continue_from = previous_task:A`:

1. resolve task A only inside the same authorized session/scope rules used by the orchestration core
2. require compatible backend = Codex
3. obtain the retained backend conversation identifier from task A; do not accept an arbitrary caller-provided Codex `thread_id`
4. start/reuse a Codex app-server runtime as needed
5. call `thread/resume` for the retained thread when necessary
6. start a **new turn** containing task B's prompt
7. record task B as a distinct Temote task
8. record the conversation lineage in bounded task metadata so later inspection can explain that B continued A
9. preserve idempotent `operation_id` handling so response loss does not create duplicate turns/tasks
10. if the retained conversation cannot be resumed, report the real state/error; do not silently fall back to a fresh thread unless the caller explicitly chose fallback behavior

The existing same-task recovery behavior must remain unchanged.

## 5. Safety / ownership constraints

Continuation must not weaken existing boundaries.

At minimum:

- no cross-session task lookup by ID
- no continuation across incompatible canonical scopes unless a later explicit design permits it
- no permission elevation through the previous task
- no reuse of another task's approval receipt as approval for the new task
- no arbitrary backend-native session/thread ID input
- no transcript inlining outside the bounded evidence contract
- no automatic continuation when the previous task belongs to another backend or unavailable execution context
- session stop / restart behavior must preserve uncertainty rather than claiming successful continuation without backend evidence

## 6. Suggested task metadata

The exact schema can be decided in the implementation packet, but the data model must be able to distinguish:

```text
task_id
backend
execution_id / generation
continued_from_task_id?      # Temote lineage
backend_conversation_id?     # internal backend record, e.g. Codex thread_id
backend_turn_id?
```

Do not overload `depends_on` to carry conversation lineage.

If the common orchestration layer later supports portable continuation across backends, that must be a separate capability; Codex thread IDs must not leak into the generic public contract.

## 7. Acceptance scenarios

- [ ] task A starts normally and receives Codex thread X
- [ ] task B starts with explicit continuation from A and gets a new Temote task ID
- [ ] task B uses thread X and starts a new turn instead of calling `thread/start`
- [ ] task B can be queried independently from task A
- [ ] task A completion does not mark task B complete
- [ ] task A verification PASS is not inherited by B
- [ ] task A delivery state is not inherited by B
- [ ] response loss/retry for B does not duplicate the turn
- [ ] cross-session `continue_from` is rejected
- [ ] backend mismatch is rejected or returns an explicit unsupported-capability result
- [ ] unavailable/non-resumable prior conversation does not silently create a fresh unrelated thread
- [ ] default start without continuation preserves current new-thread behavior
- [ ] existing same-task runtime reconstruction via `thread/resume` continues to work

## 8. Tests to add

Cover at least:

1. new task -> `thread/start`
2. continued task -> no `thread/start`, existing `thread/resume` where required, then a new `turn/start`
3. live runtime continuation and reconstructed-runtime continuation
4. idempotent replay of continued task start
5. previous task missing / wrong session / wrong scope
6. previous backend unsupported or mismatched
7. retained thread resume failure
8. independent execution / verification / delivery state for parent and child tasks
9. bounded lineage serialization / retention compatibility for old task records

## 9. Relationship to the development-harness restructure

This belongs in the orchestration layer rather than as a Codex-only public tool.

The common core should own the intent "continue from this previous Temote task"; the backend adapter maps that intent to the backend-native primitive when supported.

This keeps the existing restructure principles intact:

- logical task != execution
- task graph != PR graph
- task dependency != agent conversation lineage
- backend-native session/thread identifiers stay behind the adapter boundary
- local / MCP / HTTP / Gateway callers should eventually see the same continuation behavior through the same orchestration core

## 10. Non-goals for the first slice

- semantic automatic detection that two tasks are "related"
- automatically reusing the latest thread in a workspace
- cross-backend context migration
- transcript summarization as a substitute for native resume
- merging task records because they share an agent conversation
- changing workspace allocation or PR-stack behavior

## 11. Completion report requirements

When implemented, report separately:

- new continuation contract and backend capability representation
- Codex mapping to thread resume/new turn
- state/storage migration behavior
- tests executed and PASS / FAIL / NOT RUN
- live Codex acceptance if actually executed
- final git status and delivery commit/PR
