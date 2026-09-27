# OC1: preflight + capability-blocker classification for OpenCode tasks

Status: ready
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `c305e41`
Parent issue: `issues/open/20260927-opencode-checkout-command-execution-capability.md`
Prerequisites: none for this slice; full checkout *provisioning* stays with the
parent (Phase F/C work).

## 1. Goal

`opencode_task_start` fails fast with an explicit capability/workspace blocker
when the scoped workspace cannot host an implementation task — instead of
spending a model turn that ends in an apparent model failure.

## 2. Fixed decisions

- Preflight runs inside `task_start_with_store_and_binary`
  (`src/opencode_server.rs:3655`) after argument validation and before the
  serve child / session create side effect — the record may persist the
  accepted operation receipt, but no `opencode serve` spawn happens when the
  workspace is unusable.
- Blocker classes are distinguishable (parent §4):
  - `repository_unresolvable` — the task names a repo that cannot be resolved;
  - `checkout_missing` — scoped cwd is not a readable VCS checkout;
  - `checkout_read_only` — checkout exists but is not writable;
  - `execution_unavailable` — command execution cannot be provided;
  - `cwd_mismatch` — effective cwd does not equal the resolved checkout.
- Blocker surface: a structured `blocker` object in the task view / error
  payload with `class`, `backend: "opencode"`, resolved/attempted path,
  `missing_capability`, and a `recovery_hint`. This is a capability blocker,
  never `retryable_failed`/`failed` model framing (parent §5).
- Checkout detection uses the existing VCS surface (`src/vcs.rs`:
  `VcsBackendKind`, `VcsManager::open`, `inspect`) plus `.git`/`.jj` markers —
  no new provisioning logic in this packet.
- Sessions/tasks started where Temote legitimately has no repo checkout remain
  possible when the task does not need one: preflight classifies only when the
  task contract requires a workspace (default: implementation tasks). Keep a
  conservative default — when uncertain whether a task needs a checkout, do not
  block; log the skipped check in task metadata.

## 3. Read / change scope

- `src/opencode_server.rs`: `task_start`/`task_start_with_store_and_binary`
  (lines 3650/3655), `TaskRecord` (~416), `TaskStatus` (342) — decide whether a
  blocker is a new status or a structured `last_error`/view field; keep
  `TaskStatus` wire values stable if possible (additive view field preferred).
- `src/vcs.rs`: `VcsManager::open`/`inspect`, `VcsErrorCode` (203) for reuse.
- `src/mcp.rs` / `gateway/src/protocol.js`: only if the public schema or
  documented result shape changes — then regen contract + fingerprint.

## 4. Steps

1. Inventory what `task_start` already validates (uuid/task/model/agent/variant)
   and where `scope_cwd` is canonicalized (line ~3696).
2. Implement the blocker enum + detection helpers with unit coverage.
3. Wire preflight into the start path before side effects; persist the blocker
   detail on the record so `task_get` shows it.
4. Cover each blocker class with a fake/fixture test; keep the happy path green.
5. `just sandboxed-check`; host/live rows NOT RUN and recorded.

## 5. Acceptance

- [ ] A task targeting a workspace that is not a VCS checkout fails fast with
      `checkout_missing` (or the equivalent blocker class) before serve spawn.
- [ ] `checkout_missing` vs `execution_unavailable` vs `cwd_mismatch` are
      separately observable.
- [ ] Blocker output carries backend, path, missing capability, and a recovery
      hint — and is never presented as a model failure.
- [ ] Tasks without a checkout requirement are unaffected.
- [ ] Existing uncommitted work in the workspace is never reset/cleaned.
- [ ] Regression test covers the success path plus each blocker class.

## 6. Validation commands

- `cargo test --bin temote-mcp --all-features --locked opencode`
- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `just sandboxed-check`
- Live repro of the `f4ah6o/workflows.mbt` scenario from the parent: host/live
  gate, record in the live matrix.

## 7. Delivery authorization

One feature branch + one PR to `main`. Scope stops at detection/classification —
checkout provisioning itself is out of scope.

## 8. Completion report

(to be filled by the implementing packet run)
