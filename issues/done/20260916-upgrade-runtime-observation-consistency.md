# Upgrade friction slice 3: unify dry-run/apply runtime observation

Status: done
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

## Implementation

- Runtime-directory derivation is centralized in `runtime_root()`, which
  returns the resolved path together with a bounded `RuntimeRootSource`
  label (`temote-runtime-dir` | `xdg-runtime-dir` | `home-cache`). Every
  PID/state/legacy locator (`runtime_directory()`, `pid_file()`,
  `runtime_state_file()`) funnels through it, so dry-run and apply always
  inspect the same directory for a given environment.
- `DirectIngressUpgradePlan` carries bounded non-secret observation
  diagnostics: `runtime_root` (the derivation label), `runtime_root_id` (a
  truncated SHA-256 of the resolved directory — distinguishes same/different
  roots without revealing the path), `pid`, `host_id`, and `state_schema`.
  `RemoteUpgradePreflight` embeds the full observed plan as `direct_ingress`,
  so `upgrade --dry-run` and remote `upgrade_preflight` output the same
  classification plus diagnostics; two callers that disagree can compare the
  fields to identify a namespace/runtime-root difference instead of silently
  disagreeing.

## Verification

- `ingress_runtime_observations_report_equivalent_and_distinct_roots`
  (Linux, deterministic): equivalent observers produce identical
  source/target/action classification and identical root diagnostics;
  switching `TEMOTE_MCP_RUNTIME_DIR` yields an explicitly different
  `runtime_root_id` on the observation rather than silent disagreement.
- `cargo test --bin temote-mcp --all-features --locked lifecycle::` — 25/25
  pass; fmt, clippy `-D warnings`, and `git diff --check` clean.
- `docs/public-http.md` / `docs/public-http.ja.md` describe the bounded
  diagnostic fields.
