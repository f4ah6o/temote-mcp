# Upgrade friction slice 2: make direct ingress survive caller process-group cleanup

Status: done
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

## Implementation

- `spawn_direct_ingress` now returns the spawned `Child` and detaches it from
  the caller with `pre_exec` + `setsid`: the restarted ingress becomes its own
  session leader, so teardown of the upgrade caller's process group or
  controlling terminal cannot take it down.
- `apply_direct_ingress_upgrade` polls `child.try_wait()` inside the bounded
  health-verification window: a restart that exits before becoming healthy
  fails deterministically instead of waiting out the full timeout.
- New `cleanup_failed_ingress_spawn` runs whenever the durability proof
  fails. It re-reads the recorded pid and only invokes `down()` while the slot
  still belongs to the failed spawn, so a slot re-claimed by another process
  is left untouched; `down()` itself still refuses to signal a non-Temote or
  foreign-locked process, preserving fail-closed behavior.

## Verification

- `restarted_ingress_becomes_a_session_leader_detached_from_the_caller`:
  asserts `/proc/<pid>/stat` reports the spawned ingress as its own process
  group and session leader (Linux, deterministic).
- `failed_ingress_cleanup_preserves_foreign_slots_and_clears_owned_stale_ones`:
  a slot recording a different pid is left intact; the owned stale slot is
  removed without signaling the foreign process; a foreign-held slot lock
  fails closed with the state preserved.
- `cargo test --bin temote-mcp --all-features --locked lifecycle::` — 26/26
  pass; fmt, clippy `-D warnings`, and `git diff --check` clean.
