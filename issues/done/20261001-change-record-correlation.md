# D1: persistent Temote Change record + task/execution/workspace correlation

Status: done
Model: unknown
Created: 2026-10-01
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Persist Change identity and correlate Task, Execution, Workspace, and VCS identities without deriving delivery from agent hierarchy.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Persist Change identity and correlate Task, Execution, Workspace, and VCS identities without deriving delivery from agent hierarchy.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 8. Deferred

Change/workspace allocation (D2), delivery planning (D3), GitHub/gh-stack adapter (D4), verification/delivery lifecycle integration, and final release blocking remain in the parent.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Fixed design from source

**2. Identity rule**

Keep all identities distinct:

```text
TaskId != ChangeId != ExecutionId != WorkspaceId
       != jj change_id != materialized revision
       != Git ref != GitHub PR
```

A task dependency does not imply a change dependency.

**3. Minimum v1 record**

Persist a versioned bounded record containing at least:

- `change_id` — Temote-generated stable ID
- owning full session instance / canonical scope
- repository identity
- `task_id`
- optional `parent_change_id`
- explicit base kind: `origin_main` or `change_id`
- optional `base_change_id` when base kind is change
- optional `workspace_id`
- optional backend-native logical change identity
- optional latest materialized revision
- bounded executor history / execution IDs
- record revision / updated timestamp
- reconciliation state placeholder for later delivery packets

Leave verification/delivery/PR fields representable as optional versioned extensions, but do not implement remote delivery behavior here.

**4. Ownership / mutation**

- Read/update is scoped to the same full session instance and canonical repository/scope rules.
- Cross-session lookup by raw `change_id` fails closed.
- `parent_change_id` must resolve in the same authorized repository/session scope.
- Base relationship is explicit; do not infer it from `parent_task_id` or executor hierarchy.
- Updates use an expected record revision or equivalent compare-and-swap so concurrent correlation writes do not silently overwrite each other.
- Replaying an identical correlation update is idempotent.
- A conflicting update fails explicitly rather than re-parenting/re-basing a Change implicitly.

**5. Correlation behavior**

Provide internal operations sufficient to:

1. create a Change record for a known Task;
2. bind/unambiguously record a WorkspaceId later;
3. append an ExecutionId/executor-history entry;
4. record backend-native logical change identity / materialized revision when observed;
5. read/list the bounded Change projection for orchestration;
6. emit a safe observation seam referencing IDs/revisions only, without raw diff content.

Do not make subagent creation implicitly create a Change.

## 受け入れ条件

Complete source criteria from “6. Acceptance” (unchecked items remain unverified):

- [x] a stable ChangeId can be created and reread for an authorized task.
- [x] parent task identity and parent change identity remain independent fields.
- [x] explicit `origin_main` and `change_id` bases round-trip without inference.
- [x] workspace and execution correlations can be added idempotently.
- [x] concurrent/conflicting record updates fail closed instead of losing a correlation.
- [x] cross-session/cross-scope Change lookup/update is rejected.
- [x] record serialization is bounded and legacy deployments with no Change records continue to work.
- [x] safe observation correlation contains identifiers/revisions, not raw diff/transcript.
- [x] no VCS workspace/change/ref or GitHub object is created by this packet.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 7. Tests

- [ ] create/read/list
- [ ] task vs parent-change independence
- [ ] workspace binding replay/conflict
- [ ] execution-history append replay/conflict
- [ ] cross-session and same-id replacement isolation
- [ ] bounded record / malformed record handling
- [ ] no VCS/GitHub side-effect regression
- [ ] `cargo fmt --all -- --check` PASS
- [ ] focused tests PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 検証記録

- 2026-10-06: independent Change/task/base identities, idempotent correlation, CAS/ownership conflicts and bounded safe projections PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/change.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.

## 既存設計・履歴

> Historical Status: ready
Repository: `f4ah6o/temote-mcp`
> Historical Created: 2026-10-01 (Asia/Tokyo)
Parent: `issues/open/20260926-task-change-orchestration-stacked-pr.md`
Depends on: VCS/workspace identity contracts already on main; this packet does not allocate a workspace or deliver a PR

## 1. Goal

Introduce the durable Temote `Change` identity and correlation record that later allocation/delivery packets can use without deriving delivery topology from agent hierarchy.

This packet is persistence/correlation only. It must not create a jj change, Git branch/worktree, GitHub PR, or `gh-stack` object.

## 2. Identity rule

Keep all identities distinct:

```text
TaskId != ChangeId != ExecutionId != WorkspaceId
       != jj change_id != materialized revision
       != Git ref != GitHub PR
```

A task dependency does not imply a change dependency.

## 3. Minimum v1 record

Persist a versioned bounded record containing at least:

- `change_id` — Temote-generated stable ID
- owning full session instance / canonical scope
- repository identity
- `task_id`
- optional `parent_change_id`
- explicit base kind: `origin_main` or `change_id`
- optional `base_change_id` when base kind is change
- optional `workspace_id`
- optional backend-native logical change identity
- optional latest materialized revision
- bounded executor history / execution IDs
- record revision / updated timestamp
- reconciliation state placeholder for later delivery packets

Leave verification/delivery/PR fields representable as optional versioned extensions, but do not implement remote delivery behavior here.

## 4. Ownership / mutation

- Read/update is scoped to the same full session instance and canonical repository/scope rules.
- Cross-session lookup by raw `change_id` fails closed.
- `parent_change_id` must resolve in the same authorized repository/session scope.
- Base relationship is explicit; do not infer it from `parent_task_id` or executor hierarchy.
- Updates use an expected record revision or equivalent compare-and-swap so concurrent correlation writes do not silently overwrite each other.
- Replaying an identical correlation update is idempotent.
- A conflicting update fails explicitly rather than re-parenting/re-basing a Change implicitly.

## 5. Correlation behavior

Provide internal operations sufficient to:

1. create a Change record for a known Task;
2. bind/unambiguously record a WorkspaceId later;
3. append an ExecutionId/executor-history entry;
4. record backend-native logical change identity / materialized revision when observed;
5. read/list the bounded Change projection for orchestration;
6. emit a safe observation seam referencing IDs/revisions only, without raw diff content.

Do not make subagent creation implicitly create a Change.

## 6. Acceptance

- [ ] a stable ChangeId can be created and reread for an authorized task.
- [ ] parent task identity and parent change identity remain independent fields.
- [ ] explicit `origin_main` and `change_id` bases round-trip without inference.
- [ ] workspace and execution correlations can be added idempotently.
- [ ] concurrent/conflicting record updates fail closed instead of losing a correlation.
- [ ] cross-session/cross-scope Change lookup/update is rejected.
- [ ] record serialization is bounded and legacy deployments with no Change records continue to work.
- [ ] safe observation correlation contains identifiers/revisions, not raw diff/transcript.
- [ ] no VCS workspace/change/ref or GitHub object is created by this packet.

## 7. Tests

- [ ] create/read/list
- [ ] task vs parent-change independence
- [ ] workspace binding replay/conflict
- [ ] execution-history append replay/conflict
- [ ] cross-session and same-id replacement isolation
- [ ] bounded record / malformed record handling
- [ ] no VCS/GitHub side-effect regression
- [ ] `cargo fmt --all -- --check` PASS
- [ ] focused tests PASS
- [ ] `cargo test` PASS
- [ ] `cargo clippy --all-targets -- -D warnings` PASS
- [ ] `cargo check --no-default-features --all-targets` PASS
- [ ] `git diff --check` PASS

## 8. Deferred

Change/workspace allocation (D2), delivery planning (D3), GitHub/gh-stack adapter (D4), verification/delivery lifecycle integration, and final release blocking remain in the parent.
