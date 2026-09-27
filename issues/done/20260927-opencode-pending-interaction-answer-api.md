# OpenCode pending permission/question response API

## Status

done — implemented by PR #71 (`9963f7fca2771401abf4bf0eaab1c50bae362c6f`); repository acceptance complete, installed-runtime live canary tracked in `issues/open/20260908-live-acceptance-matrix.md`.

## Original problem (before PR #71)

Temote could run and steer an OpenCode-backed session, but it had no
first-class API for answering an individual OpenCode permission or question
request once OpenCode is waiting for that interaction.

This was not equivalent to ordinary agent steering. We confirmed that sending
`steer` while OpenCode had a pending permission/question did not satisfy the
interaction: execution returned to the same pending request.

As a result, a Temote caller could observe that OpenCode was blocked but could not
resolve the block through Temote.

## Confirmed behavior before implementation

1. OpenCode emitted a permission or question request and waited for an answer.
2. Temote could observe the backend/session state sufficiently to see that work is
   blocked on the interaction.
3. Temote exposed no operation that targets and answers that specific pending
   request.
4. Sending `steer` did not consume the permission/question request.
5. The same request remained pending and the job could not make progress without an
   out-of-band OpenCode interaction.

## Desired behavior

Expose pending backend interactions as explicit Temote resources and allow a
caller to answer the exact interaction through Temote.

The abstraction should support at least:

- listing or inspecting pending interactions for a session/job;
- a stable interaction/request identifier;
- OpenCode permission requests:
  - approve;
  - deny;
  - any scoped/persistent approval mode OpenCode actually supports;
- OpenCode question requests:
  - select an offered option;
  - submit free-form text when supported;
- correlation to the exact request being answered rather than implicitly
  answering "the current prompt";
- clear handling for stale, already-resolved, unknown, or no-longer-pending
  interaction IDs.

A possible conceptual API is:

```text
interaction_list(session/job)
interaction_answer(interaction_id, answer)
```

The final naming and transport shape should follow Temote's current public API
conventions.

## Design constraints

- Do not overload `steer` with structured approval/question semantics.
  `steer` should remain an agent-instruction mechanism.
- Do not require callers to attach directly to OpenCode or know its private
  transport details.
- Preserve backend-specific payloads only where needed; expose a small common
  Temote interaction model where practical.
- Answering must be idempotent or fail safely enough that retries cannot
  accidentally answer a different later request.
- The API must make concurrent or sequential pending interactions unambiguous.
- Existing Codex and Devin backend behavior must not regress.

## Acceptance criteria

- [x] A Temote caller can enumerate or inspect a pending OpenCode
      permission/question.
- [x] Each pending interaction has a stable identifier sufficient for safe
      correlation.
- [x] A caller can approve or deny an OpenCode permission request through
      Temote.
- [x] A caller can answer an OpenCode question through Temote.
- [x] After a valid answer, the OpenCode job resumes and does not return to the
      same pending interaction.
- [x] Answering a stale/already-resolved interaction produces a clear,
      non-destructive result.
- [x] Retry behavior cannot apply an old answer to a newer interaction.
- [x] Tests reproduce the confirmed case where `steer` leaves the same
      permission/question pending.
- [x] Regression tests cover both permission and question flows.
- [x] Codex/Devin backend tests remain green.

## Non-goals

- Replacing OpenCode's own permission policy.
- Automatically approving permissions without an explicit caller/policy
  decision.
- Treating arbitrary agent steering text as a structured interaction response.


## Completion report

Implemented by PR #71, merged as `9963f7fca2771401abf4bf0eaab1c50bae362c6f`.

Repository behavior now provided:

- `opencode_task_get` exposes bounded `pending_interactions` with stable Temote `interaction_id` values.
- OpenCode-only `opencode_task_control(action="answer")` answers the exact pending permission/question request instead of overloading `steer`.
- Permission replies support `once` / `always` / `reject`; question replies are checked against question count, multiplicity, offered labels, and custom-input policy before the upstream side effect.
- Descendant OpenCode sessions are included and replies target the owning child session rather than assuming the root session.
- Stale/already-resolved interactions are non-destructive, and invalid answers complete a durable non-applied receipt so retry does not become a false `reconciliation_required`.
- Regression coverage includes permission, question, descendant-session routing, stale/idempotent retry, invalid-answer receipt handling, and the case where `steer` leaves a structured interaction pending.
- EN/JA usage docs and the routed/public gateway contract were updated.

Review history: the first review found one P1 and two P2 correctness issues; all three were fixed and the final re-review at `f06f342` reported no blocking findings.

Validation observed on final PR head `f06f342fe1c5f5a34ae485fc28319b73020533a6`:

- Rust CI (Ubuntu): PASS
- Rust CI (macOS): PASS
- gateway: PASS
- plan: PASS
- CodeQL: PASS
- code analysis (Rust / JavaScript-TypeScript / Python / Actions): PASS

A real installed-runtime permission/question round trip was not established by this repository review. That live-only evidence is tracked in the consolidated live acceptance matrix rather than keeping this implementation issue open.
