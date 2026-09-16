# Upgrade friction slice 1: detect binary/helper generation mismatch before mutation

Status: polished
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
