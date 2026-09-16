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
