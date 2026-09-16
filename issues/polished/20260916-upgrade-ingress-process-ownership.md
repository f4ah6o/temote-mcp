# Upgrade friction slice 2: make direct ingress survive caller process-group cleanup

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-upgrade-process-group-friction.md`
Depends on: `20260916-upgrade-helper-generation-preflight.md`

## Goal

Ensure a direct-ingress restart reported as successful remains alive after the upgrade command's caller/PTY/process group exits, or fail deterministically instead of leaving stale state.

## Scope

- model ingress restart ownership explicitly;
- ensure durable child ownership or a supported supervisor handoff;
- after caller completion, verify listener/health/PID-state ownership in a bounded deterministic test harness;
- clean stale PID/state on failed durability proof without killing unrelated processes;
- preserve port-collision and host-identity fail-closed behavior.

Do not change remote reconnect protocol or plugin reload semantics.

## Acceptance

Process-group cleanup regression reproduces old failure before fix and passes after fix; stale-state and unrelated-process protections pass; `just sandboxed-check` passes.
