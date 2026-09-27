# OpenCode pending permission/question response API

## Status

done — implemented 2026-09-27 (PR #71). `opencode_task_control` gained the
`answer` action (`interaction_id` + structured `answer` object); task views
expose `pending_interactions` with stable, payload-derived IDs; steer no longer
consumes a pending interaction. Repo-local coverage:
`pending_permission_is_inspectable_answerable_idempotent_and_stale_safe`,
`pending_question_accepts_offered_and_free_form_answers`,
`invalid_interaction_answer_finishes_receipt_without_consuming_request`,
`steer_does_not_consume_pending_question_interaction` et al. in
`src/opencode_server.rs`. Live `opencode serve` parity is tracked as a
live-acceptance-matrix row.

## Problem

Temote can run and steer an OpenCode-backed session, but it currently has no
first-class API for answering an individual OpenCode permission or question
request once OpenCode is waiting for that interaction.

This is not equivalent to ordinary agent steering. We confirmed that sending
`steer` while OpenCode has a pending permission/question does not satisfy the
interaction: execution returns to the same pending request.

As a result, a Temote caller can observe that OpenCode is blocked but cannot
resolve the block through Temote.

## Confirmed behavior

1. OpenCode emits a permission or question request and waits for an answer.
2. Temote can observe the backend/session state sufficiently to see that work is
   blocked on the interaction.
3. Temote exposes no operation that targets and answers that specific pending
   request.
4. Sending `steer` does not consume the permission/question request.
5. The same request remains pending and the job cannot make progress without an
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

- [ ] A Temote caller can enumerate or inspect a pending OpenCode
      permission/question.
- [ ] Each pending interaction has a stable identifier sufficient for safe
      correlation.
- [ ] A caller can approve or deny an OpenCode permission request through
      Temote.
- [ ] A caller can answer an OpenCode question through Temote.
- [ ] After a valid answer, the OpenCode job resumes and does not return to the
      same pending interaction.
- [ ] Answering a stale/already-resolved interaction produces a clear,
      non-destructive result.
- [ ] Retry behavior cannot apply an old answer to a newer interaction.
- [ ] Tests reproduce the confirmed case where `steer` leaves the same
      permission/question pending.
- [ ] Regression tests cover both permission and question flows.
- [ ] Codex/Devin backend tests remain green.

## Non-goals

- Replacing OpenCode's own permission policy.
- Automatically approving permissions without an explicit caller/policy
  decision.
- Treating arbitrary agent steering text as a structured interaction response.
