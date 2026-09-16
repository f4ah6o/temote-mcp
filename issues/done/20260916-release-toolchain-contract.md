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

## Implementation notes (2026-09-16)

Current `main` did not already cover this: there was no `rust-toolchain.toml` or `rust-version` contract, `.github/workflows/ci.yaml` and `.github/workflows/release.yaml` both used the floating `dtolnay/rust-toolchain@stable`, and the local active stable was 1.96.1 while release CI had resolved to Clippy 1.98.0.

Changes:

- Added `rust-toolchain.toml`: channel `1.98.0`, components `rustfmt`, `clippy`, minimal profile, with the 2026.9.10 Clippy drift recorded as the motivating regression.
- Replaced the floating action with `rustup show` in `.github/workflows/ci.yaml` and `.github/workflows/release.yaml`, so both install and activate the pinned channel/components from the repository file.
- Left the cargo-dist generated `.github/workflows/release.yml` untouched; `dist build` runs inside the checkout, so its cargo/rustup proxies resolve the same `rust-toolchain.toml`.
- Added `tests/toolchain_contract.rs`: exact `X.Y.Z` channel assertion, rustfmt/clippy component assertion, workflow consumption assertion (no floating action in ci/release/generated release), and development-docs assertion for the contract and the 2026.9.10 regression.
- Documented the contract and regression in `docs/development.md` (no Japanese development doc exists in this repository).

Observed behavior:

- `rustup show` in the checkout reports `1.98.0-x86_64-unknown-linux-gnu (active) ... overridden by '.../rust-toolchain.toml'`; `rustc`/`cargo`/`clippy` report 1.98.0 after installing the pinned toolchain.
- `cargo fmt --all -- --check` passes under the pinned 1.98.0 rustfmt.
- No package versioning or CalVer behavior changed.

Gates:

- `cargo test --test toolchain_contract --locked`: PASS (3/3).
- `just sandboxed-check`: exit 0 under the pinned 1.98.0 toolchain (lib 110/110, focused bin subsets 5+3+40+6, gateway 70/70, fmt/clippy/`--no-default-features` PASS, `git diff --check` PASS).
- host/CI-only: NOT RUN (an actual GitHub Actions CI/release run, Linux nested sandbox runtime tests, full binary/local Unix-socket integration suite, ignored supervisor/process-boundary E2E).

## 2026-09-16 completion

All packet acceptance items are met repository-locally. The source issue `issues/closed/20260916-release-ci-rust-clippy-drift.md` is already superseded by this packet and stays closed.
