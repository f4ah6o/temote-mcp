# Upgrade friction slice 3: unify dry-run/apply runtime observation

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-upgrade-process-group-friction.md`
Depends on: `20260916-upgrade-ingress-process-ownership.md`

## Goal

Make local-shell and Temote-invoked upgrade dry-run/apply inspect the same runtime directory, PID/state schema, and ingress identity, or return an explicit bounded reason for a namespace/runtime-root difference.

## Scope

- centralize runtime-state locator/identity derivation used by dry-run and apply;
- add fixed non-secret diagnostic fields needed to explain observation divergence;
- test equivalent callers produce the same source/target/action classification;
- test deliberately different runtime roots fail/classify explicitly rather than silently disagree.

Do not expose secrets, raw environment, or arbitrary runtime-directory input.

## Acceptance

Focused upgrade/runtime tests pass, docs describe the bounded diagnostics, and `just sandboxed-check` passes. Fresh-client/plugin restart behavior remains live-matrix evidence.
