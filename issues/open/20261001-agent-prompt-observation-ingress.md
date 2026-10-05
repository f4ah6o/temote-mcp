# O1P: agent-side user prompt observation ingress

Status: open
Model: unknown
Created: 2026-10-01
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Observe supported direct-agent user prompts and steering through local durable ingress without collecting hidden prompts.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

Some direct user-agent turns occur outside Temote’s current observable task boundary.

## 目標

Observe supported direct-agent user prompts and steering through local durable ingress without collecting hidden prompts.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 15. Non-goals

Initial scope excludes:

- replacing Codex/Devin chat UIs
- forcing all user prompts through Temote
- generic chat archive/search
- planner or autonomous agent routing
- automatic task creation from every prompt
- hidden reasoning capture
- Shuttle integration
- multi-host raw prompt synchronization
- cloud prompt-body storage by default

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

P0/P1 are a prepared local-ingress packet, not completed implementation. For P2/P3, inspect each installed agent's actual prompt-hook surface before implementing an adapter. If a native hook cannot observe direct user turns, record `coverage=unavailable` or `partial` with its reason; never synthesize prompt observations from task metadata. Correlation and context projection follow durable ingress.

### Preserved fixed contract: 2. Decision

Keep Temote as the canonical local observation/context store, but add a **small agent-side prompt observation ingress**.

Codex / Devin integrations should emit a bounded user-instruction lifecycle event to Temote when a user-visible prompt or steer is accepted by that agent.

Do not require the user to route all prompts through Temote.

Do not move agent execution ownership into the observation layer.

Target shape:

```text
User
  |----------------------+
  v                      v
Codex                  Devin
  | prompt hook           | prompt hook
  +-----------+-----------+
              |
              v
      Temote local observer
              |
      +-------+--------+
      |                |
      v                v
 raw local events   derived context
      |                |
      +-------> memory worker
```

The hook exists only to close the causal gap between direct agent conversation and Temote-observed execution.

## 受け入れ条件

Complete source criteria from “16. Acceptance” (unchecked items remain unverified):

- [ ] Before attempting each direct-agent acceptance flow, record the installed Codex/Devin hook capability. If no supported hook exists, record `coverage=unavailable` or `partial` and an explicit unsupported gap; never mark a fabricated prompt event as PASS.

### A. Codex direct prompt

1. user sends a prompt directly to Codex
2. prompt does not pass through an ordinary Temote task API
3. Codex integration emits one `user_prompt_accepted`
4. Temote stores it locally with stable conversation/source identity
5. later Temote task/execution can be correlated to the prompt
6. `context_resolve` exposes the direct observed user intent with provenance

### B. Devin direct prompt

Same acceptance as A using Devin ACP or Devin Cloud.

### C. Cross-agent continuation

1. user instructs Codex
2. Codex performs work through Temote
3. Codex conversation stops
4. Devin later asks Temote for context
5. Devin can obtain:
   - latest observed user intent
   - completed/in-progress Temote state
   - relevant evidence/test state
   - unresolved work
   - provenance showing which facts came from direct prompt vs execution observation

No manual handoff message from Codex is required.

### D. Sensitive prompt

1. user prompt contains credential-like/private content
2. raw body is retained only in owner-only local storage
3. normal remote context/status does not expose the raw body
4. Fabric/cloud replication contains no prompt body by default
5. worker input follows local content policy

### E. Restart/idempotency

1. hook emits an event
2. delivery response is lost
3. integration retries the same source event
4. only one durable observation exists
5. observer/Temote restart preserves cursor and coverage state

### F. Missing hook

If a direct prompt surface cannot be observed, context reports `coverage=unavailable` or `partial`; it does not synthesize a prompt.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.

## 2026-10-05 実行パケット

- [`agent-prompt-hook-capabilities`](../done/20261005-agent-prompt-hook-capabilities.md)
- [`agent-prompt-correlation-context`](../done/20261005-agent-prompt-correlation-context.md)

These are planned packets, not completed implementation. The parent remains open until applicable children and acceptance evidence are complete.

## 既存設計・履歴

> Historical Status: open / design parent — P0+P1 polished as `issues/done/20261001-agent-prompt-local-ingress-contract.md`; P2-P7 remain

Repository: `f4ah6o/temote-mcp`  
Parent: `issues/open/20260925-observation-context-memory-plane.md`  
Related: `issues/closed/20260925-agent-conversation-continuation.md`
> Historical Created: 2026-10-01 (Asia/Tokyo)

## 0. Triage / next packet

Implement `issues/done/20261001-agent-prompt-local-ingress-contract.md` first. It establishes the durable local contract without taking a dependency on Codex/Devin hook mechanics. Keep this parent open for P2-P7 only.

## 1. Problem

Temote's observation plane can currently record instructions that cross the Temote execution boundary.

That is not enough for the current multi-agent usage pattern.

A user may talk directly to Codex or Devin, while those agents use Temote only for delegated execution and machine access. In that flow, the most important causal fact — **what the user asked the agent to do** — may never pass through Temote.

The result is an incomplete timeline:

```text
User -> Codex/Devin      # user intent exists here
          |
          +-> Temote     # Temote sees delegated execution only
```

Temote can observe sessions, jobs, evidence, VCS state, and backend lifecycle, but a later agent cannot reliably reconstruct why those actions happened.

This becomes especially visible when:

- Codex is interrupted and Devin continues the work
- Devin is stopped and Codex resumes later
- the original agent conversation is no longer available
- the observer/memory worker must explain current intent from durable state

## 2. Decision

Keep Temote as the canonical local observation/context store, but add a **small agent-side prompt observation ingress**.

Codex / Devin integrations should emit a bounded user-instruction lifecycle event to Temote when a user-visible prompt or steer is accepted by that agent.

Do not require the user to route all prompts through Temote.

Do not move agent execution ownership into the observation layer.

Target shape:

```text
User
  |----------------------+
  v                      v
Codex                  Devin
  | prompt hook           | prompt hook
  +-----------+-----------+
              |
              v
      Temote local observer
              |
      +-------+--------+
      |                |
      v                v
 raw local events   derived context
      |                |
      +-------> memory worker
```

The hook exists only to close the causal gap between direct agent conversation and Temote-observed execution.

## 3. Product boundary

### 3.1 Agent-side hook owns capture

The agent integration knows when a user-visible turn was accepted.

It emits only the event needed to correlate that turn with Temote state.

Initial sources:

- Codex
- Devin ACP
- Devin Cloud where a stable conversation/message event is available

Future sources may use the same ingress contract.

### 3.2 Temote owns durability and correlation

Temote owns:

- local append-only prompt observations
- correlation to host / session / repository / workspace / task / execution
- correlation to backend conversation identity when known
- coverage/freshness reporting
- context projection for the next agent
- sanitization policy before any model or cloud projection sees content

### 3.3 Coding agents do not maintain memory

Do not add instructions such as:

- "remember this"
- "write a handoff"
- "summarize your state"
- "update shared memory"

to normal coding prompts.

The observer and memory worker remain outside the coding task.

## 4. Observed events

Initial event kinds:

```text
user_prompt_accepted
user_steer_accepted
conversation_bound
conversation_unbound
prompt_observation_gap
```

Do not capture hidden chain-of-thought or private backend reasoning.

Do not capture system/developer prompts by default.

The initial scope is the explicit user-visible instruction that establishes or changes task intent.

## 5. Event envelope

Conceptual schema:

```rust
struct AgentPromptObservation {
    id: ObservationId,
    schema_version: u32,
    observed_at: Timestamp,

    host_id: HostId,
    temote_session_id: Option<SessionId>,

    agent: AgentKind,              // codex | devin
    agent_conversation_id: String,
    source_event_id: String,

    repository_key: Option<String>,
    workspace_id: Option<WorkspaceId>,
    task_id: Option<TaskId>,
    execution_id: Option<ExecutionId>,

    kind: PromptObservationKind,

    content: PromptContentRef,
    provenance: PromptProvenance,
}
```

Required identity properties:

- `agent + agent_conversation_id + source_event_id` is idempotent
- repeated delivery MUST NOT create duplicate observations
- a prompt may initially be unbound to a Temote task/execution
- later correlation may add linkage without rewriting the original observation

If a backend does not provide a stable source event id, the integration must allocate and persist one before retryable delivery.

## 6. Prompt content policy

Raw user prompts may contain:

- credentials
- tokens
- internal URLs
- customer data
- source snippets
- proprietary instructions

Therefore the captured prompt body is **local-only by default**.

The prompt observation record should separate identity from content:

```text
structural envelope
  -> safe for normal indexing

local content body
  -> owner-only host storage

digest / bounded metadata
  -> optional derived representation
```

Rules:

1. raw prompt content is stored only in the Temote owner-only state directory by default
2. ordinary remote MCP surfaces do not return raw prompt bodies
3. prompt content is not replicated to Fabric/cloud by default
4. cloud projection may carry structural metadata and a digest without the body
5. any future prompt-body cloud replication must be explicit opt-in
6. free-text secret scanning is not treated as a complete security boundary

The existing observation/content reference model should be reused rather than copying prompt bodies into multiple logs.

## 7. Memory-worker boundary

A cheap always-on model may consume prompt observations only after a deterministic local policy layer decides what content is eligible.

Target pipeline:

```text
raw local prompt observation
          |
          v
 deterministic local policy
   - content tier
   - redaction where possible
   - size bound
   - structured secret exclusion
          |
          v
 memory / context worker
          |
          v
 derived state
```

Derived state may include:

- current goal
- current task
- latest user intent
- completed work
- in-progress work
- blockers
- decisions
- changed files
- test results
- next actions
- last active agent

Derived state is never authoritative. It must remain rebuildable from retained observations and Temote authoritative execution state.

## 8. Correlation model

Direct user prompts can arrive before a Temote task exists.

The observer must support late binding.

Example:

```text
T0 user_prompt_accepted
   agent=codex
   conversation=thread-123
   task_id=null

T1 Codex invokes Temote and starts Task A

T2 correlation links:
   thread-123 prompt -> Task A -> Execution E
```

Correlation signals, in priority order:

1. explicit backend conversation/thread identity already retained by Temote
2. explicit task/execution identity supplied by the integration
3. Temote session identity
4. repository/workspace identity + bounded temporal correlation

Do not silently claim an exact task correlation from timing alone.

If only weak correlation is available, expose it as partial/uncertain rather than authoritative.

## 9. Codex integration

Use the existing Codex app-server/plugin integration boundary where possible.

The implementation should determine the narrowest stable hook that can observe a user-visible accepted turn without proxying the entire Codex UI through Temote.

Requirements:

- obtain stable Codex conversation/thread identity
- obtain stable turn/message identity when available
- record user prompt/steer acceptance
- preserve native Codex conversation continuation behavior
- do not require the user to start coding work through a new Temote-specific prompt command

If the currently loaded Codex surface cannot expose direct user turns, report coverage as unavailable rather than fabricating events.

## 10. Devin integration

Apply the same contract to Devin ACP and Devin Cloud where supported.

Requirements:

- retain Devin session/conversation identity
- use stable message/turn identity where available
- capture explicit user-visible prompts/steers
- correlate later Temote delegated work to that conversation
- do not rely on model-generated summaries as the only source of user intent

If ACP and Cloud provide different event surfaces, implement adapters behind the same Temote ingress contract.

## 11. Ingress contract

Prefer one internal/native Temote ingress rather than agent-specific storage.

Conceptual operation:

```text
observe_agent_prompt(
  agent,
  agent_conversation_id,
  source_event_id,
  kind,
  content_ref_or_local_body,
  optional correlation
)
```

This operation must:

- authenticate as a local/installed Temote integration
- validate size/content tier
- append idempotently
- return the durable observation id
- never start or mutate a coding task as a side effect

A remote public write tool for arbitrary prompt injection is explicitly not required.

## 12. Context resolver changes

`context_resolve` should be able to surface:

```text
latest_user_intent
recent_user_steers[]
active_agent_conversations[]
prompt_observation_coverage
prompt_observation_freshness
prompt_refs[]
```

The resolver must distinguish:

- direct observed user prompt
- Temote-routed instruction
- derived worker summary
- inferred/partial correlation

Do not collapse these into one unqualified "current prompt" field.

## 13. Coverage / failure semantics

Prompt observation is useful only if missing coverage is visible.

Expose at least:

```text
agent
conversation_id
coverage = complete | partial | unavailable
last_observed_at
last_source_event_id
gap_detected
```

Failure rules:

- observer delivery failure MUST NOT fail the coding task
- prompt observation retry MUST be idempotent
- restart MUST resume without duplicating accepted prompt events
- an unavailable hook MUST be reported as unavailable
- context resolver MUST NOT imply complete conversation history when coverage is partial
- losing derived summaries is recoverable from raw local observations

## 14. Security invariants

- raw prompt bodies remain host-local by default
- no credential/token fields are copied into ordinary diagnostics
- no hidden chain-of-thought capture
- no full arbitrary chat transcript scraping
- no implicit cloud prompt-body replication
- no cross-repository prompt reuse without explicit repository correlation
- no cross-user/owner visibility
- normal Temote job/session listing remains bounded and does not become a raw transcript API

## 15. Non-goals

Initial scope excludes:

- replacing Codex/Devin chat UIs
- forcing all user prompts through Temote
- generic chat archive/search
- planner or autonomous agent routing
- automatic task creation from every prompt
- hidden reasoning capture
- Shuttle integration
- multi-host raw prompt synchronization
- cloud prompt-body storage by default

## 16. Acceptance

### A. Codex direct prompt

1. user sends a prompt directly to Codex
2. prompt does not pass through an ordinary Temote task API
3. Codex integration emits one `user_prompt_accepted`
4. Temote stores it locally with stable conversation/source identity
5. later Temote task/execution can be correlated to the prompt
6. `context_resolve` exposes the direct observed user intent with provenance

### B. Devin direct prompt

Same acceptance as A using Devin ACP or Devin Cloud.

### C. Cross-agent continuation

1. user instructs Codex
2. Codex performs work through Temote
3. Codex conversation stops
4. Devin later asks Temote for context
5. Devin can obtain:
   - latest observed user intent
   - completed/in-progress Temote state
   - relevant evidence/test state
   - unresolved work
   - provenance showing which facts came from direct prompt vs execution observation

No manual handoff message from Codex is required.

### D. Sensitive prompt

1. user prompt contains credential-like/private content
2. raw body is retained only in owner-only local storage
3. normal remote context/status does not expose the raw body
4. Fabric/cloud replication contains no prompt body by default
5. worker input follows local content policy

### E. Restart/idempotency

1. hook emits an event
2. delivery response is lost
3. integration retries the same source event
4. only one durable observation exists
5. observer/Temote restart preserves cursor and coverage state

### F. Missing hook

If a direct prompt surface cannot be observed, context reports `coverage=unavailable` or `partial`; it does not synthesize a prompt.

## 17. Implementation order

```text
P0 contract + local event type
P1 local durable ingress + idempotency
P2 Codex hook
P3 Devin ACP/Cloud hook
P4 late correlation with Task/Execution
P5 context_resolve coverage/user-intent projection
P6 local worker eligibility/sanitization policy
P7 cross-agent live acceptance
```

Do not make cloud prompt-body replication a prerequisite for P0-P7.

## 18. Relationship to existing observation work

This issue is a child extension of the existing observation/context/memory plane.

The parent already defines the important invariants:

- raw observation vs derived knowledge
- Temote authoritative execution state
- rebuildable memory worker projection
- provenance/support references
- bounded content references

This child exists specifically because the parent currently limits capture to **Temote's observable execution boundary**.

The new contract extends observability one step outward through installed agent integrations, while keeping Temote as the local durable owner and preserving the rule that hidden/private reasoning is not collected.

## 19. Completion report requirements

When implemented, report separately:

- Codex capture surface actually used
- Devin capture surface actually used
- stable conversation/message identity used for idempotency
- local prompt content storage policy
- correlation behavior and uncertainty semantics
- tests executed with PASS / FAIL / NOT RUN
- live Codex acceptance if actually executed
- live Devin acceptance if actually executed
- final git status and delivery commit/PR
