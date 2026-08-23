# Crate dependency reduction

## Problem

Temote MCP is already a single crate with an optional `network` feature, but the default build still carries a relatively broad dependency graph. On the current macOS dependency graph, the default build contains about 146 non-root packages versus about 43 with `--no-default-features`. The largest reduction opportunity is therefore inside the default network feature.

The goal is not to replace mature crates indiscriminately. Dependency removal should consolidate functionality onto primitives that are already required, keep security boundaries explicit, and preserve CLI/runtime behavior.

## Plan

### Phase 1 — consolidate OAuth hashing and randomness onto `ring`

- Replace direct `sha2` use in local OAuth / PKCE with `ring::digest` SHA-256.
- Replace direct `getrandom` use for OAuth token generation with `ring::rand::SystemRandom`.
- Add `ring` as the direct optional dependency owned by the `network` feature.
- Remove the direct `sha2` and `getrandom` dependencies.
- Preserve token size, URL-safe encoding, PKCE semantics, and failure behavior.
- Keep or extend tests/PBT around token/challenge invariants.

### Phase 2 — internalize platform path resolution

- Replace `dirs` with a small internal platform-path module covering only the paths Temote actually uses (`home`, `config`, `state`, local data fallback).
- Preserve documented macOS `~/.config/temote-mcp` behavior and Linux/XDG behavior.

### Phase 3 — replace generic JWT dependency with focused Cloudflare Access verification

- Replace `jsonwebtoken` only after locking the authentication boundary with fixtures and PBT.
- Implement the narrowly required RS256/JWKS verification using existing `ring`, `base64`, and `serde_json` primitives.
- Preserve issuer, audience, expiry, not-before, algorithm, key-id, malformed-token, and size-limit checks.

### Phase 4 — remove small terminal-secret helper dependency

- Replace `rpassword` with a narrowly scoped terminal echo guard using the existing platform primitives where safe.
- Preserve non-interactive failure behavior and zeroization.

### Later candidates

- Evaluate `uuid`, `similar`, and `dotenvy` only where replacement remains simpler than the dependency.
- Evaluate `clap` separately: it has a meaningful subtree, but Temote's CLI is a public product interface and should not be weakened merely to reduce package count.
- Do not prioritize removal of `anyhow`, `axum`, or `reqwest` without a larger architectural reason; their current replacement cost is disproportionate to dependency savings.

## Acceptance

### Phase 1

- [x] `sha2` is no longer a direct dependency.
- [x] `getrandom` is no longer a direct dependency.
- [x] local OAuth PKCE challenges remain SHA-256 based and deterministic for a given verifier.
- [x] OAuth token generation remains cryptographically secure and returns the same 32-byte URL-safe-no-pad shape.
- [x] targeted local OAuth tests pass.
- [x] `cargo check --all-targets --all-features` passes.
- [x] `cargo check --all-targets --no-default-features` passes.
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [x] full tests pass.
- [x] `git diff --check` passes.

## Baseline

Measured before Phase 1 on macOS:

- default build: about 146 non-root packages
- `--no-default-features`: about 43 non-root packages
- direct `sha2` subtree: about 10 packages, roughly 8 unique to that root in the current graph
- direct `getrandom` dependency: version 0.3.x; another `getrandom` remains transitively required by cryptographic/runtime dependencies

The package-count target is directional rather than an acceptance contract because Cargo feature unification and upstream dependency versions can change the exact count.

## Phase 1 evidence

Completed on 2026-08-23 against `main` starting at `8789b00`.

- direct `sha2` and `getrandom` dependencies removed from `Cargo.toml`
- `ring` promoted to the direct optional cryptographic primitive for the `network` feature
- PKCE S256 uses `ring::digest::SHA256`; RFC 7636 example remains green
- OAuth random tokens use `ring::rand::SystemRandom` and retain the 32-byte / 43-character unpadded base64url shape
- local OAuth tests: 23 passed / 0 failed
- full tests: 302 passed / 0 failed / 1 intentional process-boundary E2E ignored
- all-target/all-feature check: pass
- no-default-features check: pass (existing dead-code warnings only)
- Clippy with `-D warnings`: pass
- `git diff --check`: pass
- macOS normal dependency graph: 146 -> 133 non-root packages (-13)
- macOS `--no-default-features`: 43 -> 43 non-root packages
