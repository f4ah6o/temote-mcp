# OC1: preflight + capability-blocker classification for OpenCode tasks

Status: ready (revised 2026-09-27 per PR #73 review: narrowed to derivable,
non-destructive probes; both hard blockers are structured pre-acceptance
errors — no record exists yet, and a persisted hard blocker would be a
non-terminal `accepted` record that `task_get` would re-drive into
`ensure_runtime_with_binary`).
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `c305e41`
Parent issue: `issues/open/20260927-opencode-checkout-command-execution-capability.md`
Prerequisites: none for this slice. The parent's typed repository/workspace
requirement (a `repository`/`requires_workspace` input on
`opencode_task_start`) is NOT part of this packet — it is a separate contract
packet; see "Deferred" below.

## 1. Goal

`opencode_task_start` fails fast — or marks the task with an explicit
capability/workspace blocker — using only information derivable from the
current tool contract (`session_id`, `operation_id`, `task`, `model`, `agent`,
`variant`; `src/mcp.rs:785`) and the session-derived `scope_cwd`
(`src/opencode_server.rs:3696`, `config::canonical_directory(&session.cwd)`),
instead of spending a model turn that ends in an apparent model failure.

## 2. Fixed decisions

- Preflight runs inside `task_start_with_store_and_binary`
  (`src/opencode_server.rs:3655`) after argument validation and *before the
  `TaskRecord` is constructed/persisted*: hard blockers abort before the
  record exists, so there is no retained `accepted` record to reconcile or
  re-drive. (A hard blocker persisted as `accepted`+`blocker` would be
  non-terminal — the next `task_get` would retry `ensure_runtime_with_binary`
  and could mutate the record to `unknown`; and `TaskStatus` has no `blocked`
  value. Pre-acceptance avoids that state-machine change.)
- All checks are derivable from the current contract and *non-provisioning,
  non-destructive*: no VCS registry/directory creation (`VcsManager::open`,
  `src/vcs.rs:325`, is explicitly NOT a detector — it requires a managed root
  and creates `.temote` registry directories), no mutation of workspace
  contents. The two probes do have bounded process/FS effects — the binary
  probe spawns a short-lived `--version` child, and the writability probe
  creates+unlinks a tempfile via `create_new` with cleanup-on-error (unlink on
  drop/all exits).
- Blocker classes for this slice:
  - `scope_unresolvable` — `scope_cwd` cannot be canonicalized / does not
    exist. **Hard blocker**, delivered as a *structured pre-acceptance error*:
    `TaskRecord.scope_cwd` is populated by
    `config::canonical_directory(&session.cwd)?` while constructing the record
    (`src/opencode_server.rs:3696`), so on failure no TaskRecord/receipt can
    exist — this class is returned in the tool error payload (same blocker
    shape: class, backend, attempted path, missing_capability, recovery_hint)
    and is never persisted as a task.
  - `execution_unavailable` — the resolved serve binary cannot be probed
    (spawn/`--version` fails, times out, or is not executable), reusing the
    `CommandRunner`-style probe pattern of `tool_capability`
    (`src/vcs.rs:948`). **Hard blocker**, delivered as a *structured
    pre-acceptance error* exactly like `scope_unresolvable` — preflight runs
    before record construction, so no task record or receipt is persisted.
  - `checkout_missing` — `scope_cwd` is not inside a VCS worktree, detected
    read-only via `git -C <scope_cwd> rev-parse --is-inside-work-tree`
    (CommandRunner probe) with a `.git`/`.jj` marker fallback — no
    `VcsManager` open. **Advisory**: recorded as a classified blocker on the
    task; does not refuse start.
  - `checkout_read_only` — `scope_cwd` fails a writability probe (create +
    unlink of a namespaced tempfile inside the scope; documented in code as a
    probe, never touching user files). **Advisory**.
- Checkout classes are advisory rather than refusing because the current
  contract cannot express "this task requires a workspace" — there is no
  `repository`/`requires_workspace` input, so a hard refusal would
  misclassify legitimate non-repository tasks. They become refusal-grade in
  the follow-up that adds the typed input.
- Blocker surface: a structured `blocker` object with `class`,
  `backend: "opencode"`, resolved/attempted path, `missing_capability`, and a
  `recovery_hint`. Advisory classes (`checkout_missing`,
  `checkout_read_only`) are persisted on the task record and surfaced in the
  task view / operation receipt — prefer an additive view field over a new
  `TaskStatus` wire value (`src/opencode_server.rs:342`). Hard classes are
  structured pre-acceptance errors only. This is a capability blocker, never
  `retryable_failed`/`failed` model framing (parent §5).
- Idempotent replay note: a hard blocker persists no receipt, so replaying the
  same `operation_id` re-runs preflight and deterministically returns the same
  structured error — consistent with idempotent start semantics.
- The blocker enum/serialization is extensible: `repository_unresolvable` and
  `cwd_mismatch` must be addable later without a schema break.

### Deferred (explicit — not this packet)

- `repository_unresolvable` (task names a repo that cannot be resolved) and
  `cwd_mismatch` (effective cwd ≠ resolved checkout): both require a typed
  repository/workspace input that does not exist on `opencode_task_start`.
  Follow-up packet: extend the tool contract (+ gateway regen) and upgrade
  `checkout_missing`/`checkout_read_only` to refusal-grade when the input
  declares a workspace requirement.
- Actual checkout provisioning (Phase F/C in the parent).

## 3. Read / change scope

- `src/opencode_server.rs`: `task_start`/`task_start_with_store_and_binary`
  (3650/3655), `scope_cwd` derivation (3696), `TaskRecord` (~416), `TaskStatus`
  (342) — additive view field preferred over a new status value.
- `src/vcs.rs`: `tool_capability` (948) as the probe pattern; `VcsErrorCode`
  (203) names for reuse. Do NOT call `VcsManager::open`/`inspect` here.
- `src/mcp.rs` / `gateway/src/protocol.js`: only if the public schema or
  documented result shape changes — then regen contract + fingerprint.

## 4. Steps

1. Inventory what `task_start` already validates and where `scope_cwd` is
   canonicalized (line 3696).
2. Implement the blocker enum + detection helpers with unit coverage (probe
   injection via the existing CommandRunner-style seam).
3. Wire preflight into the start path before side effects; hard blockers
   refuse before serve spawn, advisory classes persist the `blocker` record so
   `task_get` shows it.
4. Cover each class with a fake/fixture test; keep the happy path green,
   including a non-checkout scope that still starts with an advisory record.
5. `just sandboxed-check`; host/live rows NOT RUN and recorded.

## 5. Acceptance

- [ ] A start whose `scope_cwd` no longer exists fails fast with a structured
      `scope_unresolvable` pre-acceptance error before serve spawn — no task
      record or operation receipt is persisted.
- [ ] A start with an unspawnable/unprobesable serve binary fails fast with a
      structured `execution_unavailable` pre-acceptance error — no task record
      or operation receipt is persisted, and a later `task_get` for that
      `task_id` returns not-found (the blocker cannot mutate into a
      model/runtime failure).
- [ ] A start in a non-checkout scope still starts and carries a classified
      `checkout_missing` advisory record on the task view.
- [ ] `scope_unresolvable` vs `execution_unavailable` vs `checkout_missing` vs
      `checkout_read_only` are separately observable.
- [ ] Blocker output carries backend, path, missing capability, and a recovery
      hint — and is never presented as a model failure.
- [ ] No VCS registry/provisioning side effects occur during preflight; probe
      side effects are bounded (process spawn with timeout; `create_new`
      tempfile with unlink-on-error cleanup).
- [ ] Existing uncommitted work in the workspace is never reset/cleaned.
- [ ] Regression tests cover the success path plus each blocker class.

## 6. Validation commands

- `cargo test --bin temote-mcp --all-features --locked opencode`
- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `just sandboxed-check`
- Live repro of the `f4ah6o/workflows.mbt` scenario from the parent: host/live
  gate, record in the live matrix.

## 7. Delivery authorization

One feature branch + one PR to `main`. Scope stops at detection/classification
within the current tool contract — no schema change to `opencode_task_start`,
no checkout provisioning, no refusal on advisory classes.

## 8. Completion report

(to be filled by the implementing packet run)
