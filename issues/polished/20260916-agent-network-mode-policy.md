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
