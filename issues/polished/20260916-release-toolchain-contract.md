# Pin the Rust/Clippy contract used by normal CI and release CI

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Source issue: `issues/closed/20260916-release-ci-rust-clippy-drift.md`
Depends on: Phase 0 OpenCode canary

## Goal

Make local development, normal CI, and release CI use one repository-managed Rust/Clippy toolchain contract so a release-only floating `stable` lint cannot fail after normal CI passed.

## Scope

- Add the narrow repository-managed toolchain source of truth.
- Make normal CI and release setup consume it.
- Add/adjust deterministic tests or workflow assertions proving both paths agree.
- Record the 2026.9.10 failure as the motivating regression.

Do not change package versioning or CalVer behavior. Do not hand-edit cargo-dist generated behavior beyond the supported regeneration path.

## Acceptance

- local `cargo clippy --all-targets --all-features -- -D warnings` and CI/release use the same toolchain contract;
- the toolchain source is visible in-repo;
- normal CI would catch a new lint before the `latest` release trigger;
- relevant workflow/config tests pass;
- `just sandboxed-check` passes with host-only checks still reported NOT RUN.

## Completion

Move this packet to `issues/done/` and move/supersede the source issue after parent review.
