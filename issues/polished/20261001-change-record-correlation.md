# D1: persistent Temote Change record + task/execution/workspace correlation

Status: ready
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
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
