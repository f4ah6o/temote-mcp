# Friction observer / publisher remaining track

Status: open
Model: unknown
Created: 2026-09-26
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Classify recurring Temote friction from scoped observations and prepare an authorized, reviewable improvement publication path.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

Current observation and memory paths do not yet produce scoped friction candidates or authorized publication.

## 目標

Classify recurring Temote friction from scoped observations and prepare an authorized, reviewable improvement publication path.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 5. Implementation order and non-goals

1. F1 input identity/checkpoint and deterministic episode fixtures.
2. F1 classifier/candidate retention and owner-only diagnostics.
3. F2 scoped publication/outbox/reconciliation and failure-injection tests.
4. Live acceptance with explicit credentials and export/write authorization.

Do not add public MCP tools initially. Do not replace O3/O4, add autonomous bug fixing/task steering, publish target-project issues, collect hidden reasoning or aggregate private cross-repository content.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

## 受け入れ条件

Complete source criteria from “3. F2 — authorized publisher with durable reconciliation” (unchecked items remain unverified):

The publisher is a separate consumer with explicit Git/GitHub write authorization limited to the Temote repository. Observation ingestion or a coding-task start does not grant publication authority. Candidate support is not permission to release private source content.

1. Validate candidate eligibility, support freshness, publication authorization and redaction/export policy.
2. Search existing `issues/open/` entries and retained publication receipts by scoped fingerprint; semantic matching may suggest a match but cannot bypass identity/scope checks.
3. For a duplicate, add permitted recurrence evidence to the existing entry; otherwise prepare dated Markdown with observation, impact, expected behavior, hypothesis and acceptance criteria.
4. Use a dedicated Temote workspace and focused branch/PR. Never expand the target coding project's filesystem roots.
5. Persist publication identity/receipt before each external side effect. After an uncertain GitHub response, reconcile branch/commit/PR state before retrying; do not blindly create a second PR.
6. Keep publication pending/retryable on failure. Track candidate-consumption progress independently from successful publication; retain an outbox/receipt so advancing a scan cannot lose an unpublished candidate.
7. Require review. Never auto-merge generated friction PRs.

Owner-only source content remains owner-only. Public artifacts use explicitly safe summaries and stable provenance handles; opaque or expired references are not publicly retrievable proof. Do not place tokens, raw prompts, local paths or private excerpts into PR bodies.

### Acceptance

- [ ] Missing write authorization/export permission fails closed and retains the candidate.
- [ ] Duplicate delivery and response-loss retry create at most one logical issue/PR.
- [ ] Crash after each external side effect reconciles the existing publication safely.
- [ ] Support expiry or redaction failure blocks publication without affecting task success.
- [ ] Publisher failure never changes coding-task state or memory checkpoint.
- [ ] Generated PRs stay unmerged until an authorized review/merge action.

### F1 — scoped friction consumer and supported candidates

1. Reuse the observation source and the existing bounded processing/fencing patterns. Keep a separate consumer checkpoint so friction progress or failure never advances or rewinds memory extraction.
2. Select bounded episodes using deterministic signals: repeated retries/reconciliation, recovery churn, approval round trips, excessive status probes or repeated backend workarounds.
3. Classify each episode as `temote_friction`, `target_repository_bug`, `upstream_transient`, `insufficient_evidence` or `known_existing_issue`.
4. Retain supported candidates with evidence references and acceptance criteria. A signal alone is not a conclusion; a model assertion is not verified state.

### Source and checkpoint authority

- Local source identity includes the full session instance (host/session/process generation) and canonical authorized scope, not reusable `session_id` alone. Its cursor is the local observation revision.
- Fabric uses its existing owner/repository replication identity and cloud sequence. Preserve source provenance; do not create an unscoped global revision or substitute local revision for cloud sequence.
- Consumer identity, producer version/generation and fencing belong to the checkpoint/dedupe key. Replayed batches or stale producers cannot duplicate or overwrite newer outputs.
- Append notifications are hints. Recovery scans must rediscover unprocessed input after restart or dropped notification.
- One corrupt/degraded source must not block other sources. Expose bounded last-processed/stale/degraded status without raw bodies.

### Candidate contract

Retain `id`, scoped `fingerprint`, `status`, `scope`, `classification`, `summary`, `impact`, `expected_behavior`, `resolution_hypothesis`, `acceptance_criteria[]`, `support_observation_refs[]`, optional `support_evidence_refs[]`, `producer`, `produced_at` and recurrence metadata.

Candidates are derived and rebuildable. Require support refs and distinguish observed facts from hypotheses. Expired/unresolvable evidence must downgrade publication eligibility rather than fabricate proof. Do not copy private transcript bodies into ordinary MCP output or a GitHub artifact. Cross-repository recurrence may use authorized aggregate signals, never combine private source content across scopes.

Only supported `temote_friction` candidates qualify for new publication. Existing-issue candidates qualify only for a scoped recurrence update. Target-project bugs, transient upstream outages and insufficient evidence do not produce Temote issues.

### Acceptance

- [ ] A same-batch replay and consumer restart do not duplicate candidates.
- [ ] A same-name replacement session cannot inherit the previous instance's cursor or private content.
- [ ] Every publishable candidate has resolvable support refs and concrete acceptance criteria.
- [ ] Classification fixtures exclude target-project bugs, one-off outages and unsupported claims.
- [ ] Stale producer writes fail closed; memory checkpoints remain unchanged.
- [ ] Consumer failure does not steer/interrupt/fail a coding task.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd gateway && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.

## 2026-10-05 実行パケット

- [`friction-candidate-consumer`](../polished/20261005-friction-candidate-consumer.md)
- [`friction-authorized-publisher`](../polished/20261005-friction-authorized-publisher.md)

These are planned packets, not completed implementation. The parent remains open until applicable children and acceptance evidence are complete.

## 既存設計・履歴

> Historical Status: proposed remaining packets; supersedes the stale O3a/O3b proposal in PR #61
Repository: `f4ah6o/temote-mcp`
Parent: `issues/open/20260925-observation-context-memory-plane.md`
Implemented baseline: `issues/done/20260926-cloud-observation-knowledge-plane.md`
> Historical Created: 2026-09-26; reconciled: 2026-10-02 (Asia/Tokyo)

## 1. Goal and completed baseline

Keep ordinary coding tasks focused on their target project. Temote records observable activity; independent consumers may classify recurring Temote friction and prepare a reviewable improvement PR.

O1–O4 and the Fabric memory path are repository-locally implemented/qualified. Do not reopen their completed checkboxes, replace their storage model, or move the implemented memory extractor into a new scheduler. Cloudflare remote qualification remains in `issues/open/20260908-live-acceptance-matrix.md`.

The remaining scope is friction classification, candidate retention and authorized publication. No runtime worker/publisher is implemented by this document.

## 2. F1 — scoped friction consumer and supported candidates

1. Reuse the observation source and the existing bounded processing/fencing patterns. Keep a separate consumer checkpoint so friction progress or failure never advances or rewinds memory extraction.
2. Select bounded episodes using deterministic signals: repeated retries/reconciliation, recovery churn, approval round trips, excessive status probes or repeated backend workarounds.
3. Classify each episode as `temote_friction`, `target_repository_bug`, `upstream_transient`, `insufficient_evidence` or `known_existing_issue`.
4. Retain supported candidates with evidence references and acceptance criteria. A signal alone is not a conclusion; a model assertion is not verified state.

### Source and checkpoint authority

- Local source identity includes the full session instance (host/session/process generation) and canonical authorized scope, not reusable `session_id` alone. Its cursor is the local observation revision.
- Fabric uses its existing owner/repository replication identity and cloud sequence. Preserve source provenance; do not create an unscoped global revision or substitute local revision for cloud sequence.
- Consumer identity, producer version/generation and fencing belong to the checkpoint/dedupe key. Replayed batches or stale producers cannot duplicate or overwrite newer outputs.
- Append notifications are hints. Recovery scans must rediscover unprocessed input after restart or dropped notification.
- One corrupt/degraded source must not block other sources. Expose bounded last-processed/stale/degraded status without raw bodies.

### Candidate contract

Retain `id`, scoped `fingerprint`, `status`, `scope`, `classification`, `summary`, `impact`, `expected_behavior`, `resolution_hypothesis`, `acceptance_criteria[]`, `support_observation_refs[]`, optional `support_evidence_refs[]`, `producer`, `produced_at` and recurrence metadata.

Candidates are derived and rebuildable. Require support refs and distinguish observed facts from hypotheses. Expired/unresolvable evidence must downgrade publication eligibility rather than fabricate proof. Do not copy private transcript bodies into ordinary MCP output or a GitHub artifact. Cross-repository recurrence may use authorized aggregate signals, never combine private source content across scopes.

Only supported `temote_friction` candidates qualify for new publication. Existing-issue candidates qualify only for a scoped recurrence update. Target-project bugs, transient upstream outages and insufficient evidence do not produce Temote issues.

### Acceptance

- [ ] A same-batch replay and consumer restart do not duplicate candidates.
- [ ] A same-name replacement session cannot inherit the previous instance's cursor or private content.
- [ ] Every publishable candidate has resolvable support refs and concrete acceptance criteria.
- [ ] Classification fixtures exclude target-project bugs, one-off outages and unsupported claims.
- [ ] Stale producer writes fail closed; memory checkpoints remain unchanged.
- [ ] Consumer failure does not steer/interrupt/fail a coding task.

## 3. F2 — authorized publisher with durable reconciliation

The publisher is a separate consumer with explicit Git/GitHub write authorization limited to the Temote repository. Observation ingestion or a coding-task start does not grant publication authority. Candidate support is not permission to release private source content.

1. Validate candidate eligibility, support freshness, publication authorization and redaction/export policy.
2. Search existing `issues/open/` entries and retained publication receipts by scoped fingerprint; semantic matching may suggest a match but cannot bypass identity/scope checks.
3. For a duplicate, add permitted recurrence evidence to the existing entry; otherwise prepare dated Markdown with observation, impact, expected behavior, hypothesis and acceptance criteria.
4. Use a dedicated Temote workspace and focused branch/PR. Never expand the target coding project's filesystem roots.
5. Persist publication identity/receipt before each external side effect. After an uncertain GitHub response, reconcile branch/commit/PR state before retrying; do not blindly create a second PR.
6. Keep publication pending/retryable on failure. Track candidate-consumption progress independently from successful publication; retain an outbox/receipt so advancing a scan cannot lose an unpublished candidate.
7. Require review. Never auto-merge generated friction PRs.

Owner-only source content remains owner-only. Public artifacts use explicitly safe summaries and stable provenance handles; opaque or expired references are not publicly retrievable proof. Do not place tokens, raw prompts, local paths or private excerpts into PR bodies.

### Acceptance

- [ ] Missing write authorization/export permission fails closed and retains the candidate.
- [ ] Duplicate delivery and response-loss retry create at most one logical issue/PR.
- [ ] Crash after each external side effect reconciles the existing publication safely.
- [ ] Support expiry or redaction failure blocks publication without affecting task success.
- [ ] Publisher failure never changes coding-task state or memory checkpoint.
- [ ] Generated PRs stay unmerged until an authorized review/merge action.

## 4. Ordinary agent guidance

Do not require ordinary coding agents to switch repositories, widen roots, maintain memory, file Temote issues or open friction PRs merely because Temote added friction. Preserve exact error/state and reconcile uncertain side effects while completing the requested task.

Until F1/F2 exist, automatic friction feedback is unavailable. Do not silently fall back to assigning reporting work to the coding agent. Explicit user requests to report Temote friction and Temote-development/dogfood tasks retain their normal authorized issue/PR workflow and `docs/self-improvement-dogfood.md` harness.

## 5. Implementation order and non-goals

1. F1 input identity/checkpoint and deterministic episode fixtures.
2. F1 classifier/candidate retention and owner-only diagnostics.
3. F2 scoped publication/outbox/reconciliation and failure-injection tests.
4. Live acceptance with explicit credentials and export/write authorization.

Do not add public MCP tools initially. Do not replace O3/O4, add autonomous bug fixing/task steering, publish target-project issues, collect hidden reasoning or aggregate private cross-repository content.
