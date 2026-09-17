# Agent ordinary-command network policy: mode selection and sandbox wiring

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260915-agent-development-network-access.md`
Depends on: Phase 0 OpenCode canary

## Goal

Make the intended ask/agent/yolo ordinary-command network policy explicit in code and tests, using existing sandbox primitives rather than adding a new arbitrary network selector.

## Scope

- encode the permission-mode -> ordinary-command network-policy decision in one typed helper/path;
- wire `execute` and `start_command` to that decision without changing filesystem/path containment;
- `ask` remains network restricted;
- `agent` uses the existing development-enabled network sandbox profile;
- `yolo` behavior remains the existing local-only unrestricted path;
- add table-driven policy tests and Linux/macOS sandbox-construction tests that do not require external network.

Do not add destination allowlists, raw network policy input, public yolo promotion, or live LAN/provider tests in this packet.

## Acceptance

- ask/agent/yolo matrix is covered deterministically;
- protected metadata/path containment and public `without_sandbox` denial regressions stay green;
- `execute` and `start_command` share the same policy decision;
- relevant docs/AGENTS invariant are updated only to match implemented semantics;
- `just sandboxed-check` passes.

Live outbound/listen/LAN evidence belongs to the live matrix.

## Implementation notes (2026-09-16)

Current `main` did not already cover this: `run_session_command` branched only on `session.yolo()` and otherwise always called `sandbox::run`, which hard-codes the restricted network profile for both `ask` and `agent`.

Changes:

- `src/sandbox.rs`: added `CommandNetworkPolicy { Restricted, Development }` and `run_with_network_policy`; `run` now delegates with `Restricted` so structured `apply_patch`/doctor callers keep their existing behavior. `run_with_metadata_roots` threads the policy into the Linux and macOS command construction; Git paths stay `Restricted`.
- `src/sandbox/linux/policy.rs`: `for_command_with_network` sets the network mode and revalidates; `src/sandbox/linux/mod.rs`: `command` maps `CommandNetworkPolicy` to `LinuxNetworkPolicy` (`Development` -> the existing `LocalAgent` network-enabled profile).
- `src/sandbox/policy.rs` / `src/sandbox/macos.rs`: `SandboxSpec::command` takes the network flag and renders `(allow network-outbound)` only for `Development`.
- `src/config.rs`: `PermissionMode::command_network_policy()` is the single typed decision (`ask` -> `Restricted`, `agent` -> `Development`, `yolo` -> `None` = unrestricted local path).
- `src/mcp.rs`: `run_session_command` takes the permission mode and selects `sandbox::run_with_network_policy` or `sandbox::run_unrestricted` from that decision; `execute` and `start_command` share `spawn_sandboxed_command_with_controls` -> `run_session_command`. Filesystem/path containment, protected metadata, and public tool schemas are untouched.
- Docs/invariants updated to match: `AGENTS.md`, `docs/usage.md` / `.ja`, `docs/public-http.md` / `.ja`, `skills/temote-mcp/SKILL.md`, `CHANGES.md`.
- `issues/open/20260915-agent-development-network-access.md` records the repository-local completion and stays open for Phase 4 live evidence; a live row was added to the `Agent-mode development network` section of the live matrix.

Tests:

- `cargo test --bin temote-mcp --locked permission_modes_select_the_ordinary_command_network_policy`: PASS (ask/agent/yolo matrix).
- `cargo test --lib --locked ordinary_command_network_policy`: PASS (Linux policy construction: `Restricted` keeps `--unshare-net`, `Development` omits it; both seccomp filters compile).
- `cargo test --lib --locked linux_ordinary_command_network_policy_controls_host_loopback`: PASS on this host (live Linux sandbox: a `Development` command reaches a test-process 127.0.0.1 listener; the identical `Restricted` command fails). This test lives in `sandbox::linux_tests`, so `just sandboxed-check` reports it as NOT RUN even though it was executed explicitly here.
- `cargo test --bin temote-mcp --locked yolo_command_bypasses_sandbox_file_roots`: PASS (updated to `PermissionMode::Yolo`; yolo keeps the unrestricted local path).
- `cargo test --bin temote-mcp --locked public_tools_have_chatgpt_display_metadata`: PASS (`without_sandbox` still absent from the public tool list).
- `cargo test --lib --locked protected_metadata`: PASS (6/6).
- macOS `ordinary_command_network_policy_is_rendered_in_the_profile`: NOT RUN (macOS-only test; compiled only under `target_os = "macos"`).
- `just sandboxed-check`: exit 0 (lib 113/113, gateway 71/71, clippy clean under 1.98.0).
- host/CI-only: NOT RUN (macOS profile test, actual CI run, Linux nested sandbox runtime tests as reported by `sandboxed-check`, full binary/local Unix-socket integration suite, ignored supervisor/process-boundary E2E).

## 2026-09-16 completion

All repository-local acceptance items are met. Live outbound/LAN/RTSP evidence is the Phase 4 matrix row; the parent umbrella stays open only for that row.
