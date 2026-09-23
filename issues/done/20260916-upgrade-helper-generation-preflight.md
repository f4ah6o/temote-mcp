# Upgrade friction slice 1: detect binary/helper generation mismatch before mutation

Status: done
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-upgrade-process-group-friction.md`
Depends on: completion-upgrade branch audit

## Goal

Prevent replacement binary/helper generation mismatch from surfacing later as ordinary sandbox command failure.

## Scope

- identify the installed Temote binary + sandbox/helper bundle generation using existing bounded metadata/version checks;
- add upgrade preflight classification for compatible / incompatible / unavailable helper generation;
- fail before destructive handoff when incompatible;
- keep secrets and executable-path choice out of public input/output;
- add deterministic tests for old supervisor/new helper and matching bundle cases.

Do not change direct ingress process ownership or client reconnect in this packet.

## Acceptance

Preflight returns a fixed bounded classification before handoff, existing same-PID/session restore tests remain green, and `just sandboxed-check` passes.

## Implementation — 2026-09-22 (Devin session)

- `src/sandbox/linux/policy.rs`: `LINUX_SANDBOX_POLICY_VERSION` is now a named
  constant (still `1`) used at every `version` site, so the emitted policy
  schema generation has a single source.
- `src/sandbox/linux/helper.rs`: `temote-linux-sandbox --capabilities` prints
  `{"version","policy_schema"}` and exits without touching the sandboxed
  command path; any other argv still requires `--policy ... --`.
- `src/sandbox/linux/mod.rs`: `helper_candidates` / `helper_sibling_of`
  expose the bundle-sibling lookup so preflight resolves the helper next to
  the candidate executable the same way the running sandbox does.
- `src/session_control.rs`: `HelperGeneration` (`compatible` / `incompatible`
  / `unavailable`). `classify_helper_generation` applies the same bounded
  metadata checks as `inspect_upgrade_executable` (canonical file, owner-only,
  executable, size cap, bounded `--capabilities` stdout) and compares the
  reported `policy_schema` to the running supervisor's schema. Missing or
  uninspectable helpers classify `unavailable`; non-Linux bundles classify
  `compatible` because no helper contract exists there.
- `handle_upgrade_request` attaches `helper_generation` to the dry-run
  preview result and refuses `incompatible`/`unavailable` before plan
  building, fencing, plan-file write, or exec.
- `RemoteUpgradePreflight` carries `helper_generation` (parsed from the
  supervisor response, `unavailable` when absent), and both the local
  `upgrade` path and remote `prepare_apply` reject non-`compatible`
  classifications before admission.

## Verification — 2026-09-22

- New deterministic test `helper_generation_classifies_bundle_against_running_policy_schema`
  covers the matching bundle, an old/new-schema helper (`policy_schema: 999`),
  a missing helper, and unparseable helper output.
- New lib test `capabilities_payload_reports_running_policy_schema` pins the
  helper's reported version and schema shape.
- `cargo run --bin temote-linux-sandbox -- --capabilities` prints
  `{"policy_schema":1,"version":"2026.8.0"}`.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo check --no-default-features --all-targets --locked`, lib tests
  (sandbox linux_tests skipped: host gate), and the sandboxed-check bin
  filters (`agent_git`, `activity_job`, `activity_coverage`,
  `upgrade_transaction::tests::`, `upgrade_coordinator::tests::`,
  `session_control::tests::session_gc`) all pass; gateway `npm run
  test:sandbox` 64/64; `git diff --check` clean.
- Host-only Linux bubblewrap/userns acceptance remains a CI/host gate and was
  not run here.
