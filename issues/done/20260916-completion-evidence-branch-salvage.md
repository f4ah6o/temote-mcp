# Salvage useful completion evidence without importing stale code trees

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Depends on: activity/upgrade/app-server salvage packets
Sources: `codex/20260915-complete-open-work`, `codex/20260915-completion-evaluation`, `codex/20260915-completion-baseline`, `codex/20260915-completion-launcher`

## Goal

Preserve only still-useful evaluation/live evidence and decisions from old completion branches after implementation salvage is finished.

## Scope

- compare old evaluation documents/issue notes with current main;
- copy only non-secret evidence still relevant to current contracts;
- label evidence with original commit/date and whether current main supersedes the tested build;
- do not import old source files or obsolete issue directory state.

## Acceptance

No implementation code changes in this packet. `git diff --check` passes and every imported evidence statement points to a current owning issue/live-matrix row.

## 2026-09-22 disposition: done

- **Implementation**: added `docs/evaluations/completion-20260915-salvaged-evidence.md`, a distilled labeled copy of the still-relevant evidence from `codex/20260915-completion-evaluation`, `codex/20260915-completion-activity` (`docs/evaluations/completion-residual-validation-20260915.md`), `codex/20260915-complete-open-work`, and `codex/eval-t05-c-r2-incomplete`. Each statement carries its original commit/date and an `already-covered` / superseded / still-open label pointing at a current owning issue or live-matrix row. No source files, obsolete issue-directory state, or secrets were imported.
- **Superseded (covered on main)**: the concurrent task-ownership defect (`CODEX_TASK_RUNTIME_OWNED` + `reconciliation_deferred`), typed `resume` dead-client reuse (reconciliation-only contract + tests), and the `CODEX_APP_SERVER_INCOMPATIBLE` pin (deliberately version-agnostic).
- **Still open (live residuals)**: nested-userns `local_agent_run` on Linux, macOS Vite+ Codex acceptance, Cloudflare gateway route/secret proof, OpenCode provider entitlement, unrotated exposed credential, and the wedged-supervisor incident — attached to `issues/open/20260908-live-acceptance-matrix.md` and `issues/open/20260916-upgrade-process-group-friction.md`.
- **Verification**: `git diff --check` clean; every imported statement cites an owning issue/row; no code changes.
