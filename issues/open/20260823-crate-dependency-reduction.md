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

### Phase 2

- [x] `dirs` is no longer a direct dependency.
- [x] HOME resolution preserves non-empty `HOME` first and Unix passwd fallback.
- [x] macOS config/local-data semantics preserve `~/Library/Application Support`.
- [x] documented macOS public env and tunnel-token paths remain under `~/.config/temote-mcp`.
- [x] Linux/XDG config, state, and local-data variables are accepted only when absolute and otherwise fall back to the HOME-based XDG defaults.
- [x] targeted platform-path tests pass.
- [x] `cargo check --all-targets --no-default-features` passes.
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [x] full tests pass.
- [x] `git diff --check` passes.

### Phase 3

- [x] `jsonwebtoken` is no longer a direct or transitive dependency.
- [x] Cloudflare Access accepts only JWT header `alg=RS256` and a bounded valid `kid`.
- [x] JWKS keys must be RSA and may only declare `RS256`; modulus/exponent are decoded as base64url and verified with `ring`.
- [x] a fixed RS256 fixture verifies successfully and signing-input/signature mutation is rejected.
- [x] `exp` remains required with zero leeway and optional `nbf` retains the previous boundary semantics.
- [x] expiry/not-before behavior is checked against an independent PBT reference model.
- [x] issuer, audience, subject, email allow-list, malformed-token, and size-limit checks remain in place.
- [x] targeted Access tests pass.
- [x] `cargo check --all-targets --no-default-features` passes.
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [x] full tests pass.
- [x] `git diff --check` passes.

### Phase 4

- [x] `rpassword` and `rtoolbox` are no longer direct/transitive dependencies.
- [x] supported Unix builds read interactive secrets from `/dev/tty` without echo using the existing `libc` dependency.
- [x] terminal `ECHO`, `ECHONL`, `ICANON`, and `ISIG` are disabled only for secret entry and restored on all normal/error paths via an RAII guard.
- [x] Ctrl-C is captured while echo is hidden, terminal state is restored first, then SIGINT is re-delivered.
- [x] backspace, Ctrl-U, Ctrl-W, UTF-8 input, Enter, Ctrl-D, and common terminal escape sequences retain usable password-entry semantics.
- [x] a pseudoterminal test proves the changed termios fields are restored after guard drop.
- [x] PBT confirms generated local-mode flags only lose the four intended bits while hidden.
- [x] secret-input state-machine PBT compares generated UTF-8/edit/control/escape programs against an independent reference editor.
- [x] non-Unix builds fail closed with environment-variable guidance rather than adding a new platform dependency; release targets remain macOS/Linux.
- [x] targeted OpenAI tunnel tests pass.
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


## Phase 2 evidence

Completed on 2026-08-23 after Phase 1 commit `061e980`.

- direct `dirs` dependency removed from `Cargo.toml`
- added a focused internal `platform_paths` module for HOME/config/state/local-data resolution only
- Unix HOME fallback keeps passwd-database lookup when `HOME` is unset or empty
- macOS keeps Application Support semantics for generic config/local-data while Temote's documented public env and tunnel-token paths continue to use `~/.config/temote-mcp` explicitly
- Linux/XDG behavior keeps the prior absolute-path requirement and HOME fallbacks (`.config`, `.local/state`, `.local/share`)
- targeted platform-path tests: 2 passed / 0 failed on macOS, including XDG helper semantics and macOS Application Support semantics
- full tests: 304 passed / 0 failed / 1 intentional process-boundary E2E ignored
- no-default-features all-target check: pass (existing dead-code warnings only)
- Clippy with `-D warnings`: pass
- `git diff --check`: pass
- macOS normal dependency graph: 133 -> 130 non-root packages (-3 in Phase 2, -16 from the original baseline)
- macOS `--no-default-features`: 43 -> 40 non-root packages (-3)


## Phase 3 evidence

Completed on 2026-08-23 after Phase 2 commit `667bbe0`.

- removed `jsonwebtoken` from the `network` feature and dependency list
- removed its now-unused transitive subtree, including `simple_asn1`, `num-bigint`, and `pem` from `Cargo.lock`
- Cloudflare Access JWT verification now uses existing `ring::signature::RSA_PKCS1_2048_8192_SHA256`, `base64`, and `serde_json` primitives
- header parsing keeps RS256 and key-id fail-closed checks before key selection
- RSA JWK `kty` / optional `alg` / modulus / exponent are validated before signature acceptance
- `exp` remains mandatory; `exp < now` is rejected with zero leeway; parsed `nbf > now` is rejected
- fixed 2048-bit RS256 fixture: valid signature passes; mutated signing input and mutated signature both fail
- Access temporal PBT: 1024 generated cases against an independent boundary model
- targeted Access tests: 10 passed / 0 failed
- full tests: 305 passed / 0 failed / 1 intentional process-boundary E2E ignored
- no-default-features all-target check: pass (existing dead-code warnings only)
- Clippy with `-D warnings`: pass
- `git diff --check`: pass
- macOS normal dependency graph: 130 -> 116 non-root packages (-14 in Phase 3, -30 from the original baseline)
- macOS `--no-default-features`: 40 -> 40 non-root packages


## Phase 4 evidence

Completed on 2026-08-23 after Phase 3 commit `98e49dd`.

- removed `rpassword` from the `network` feature/dependency list and removed `rtoolbox` from `Cargo.lock`
- interactive OpenAI admin/runtime secrets now use a focused `/dev/tty` reader backed by existing `libc` termios primitives
- echo/canonical/signal handling is temporarily disabled for secret entry; a `TtyEchoGuard` restores the original changed fields on drop and explicit completion
- Ctrl-C is read as input with `ISIG` disabled, the terminal is restored, then `SIGINT` is raised so abrupt process termination cannot strand the terminal with echo disabled
- terminal editing coverage includes UTF-8, backspace, Ctrl-U, Ctrl-W, escape sequences, Ctrl-D, and content-preserving whitespace behavior
- pseudoterminal restoration test: pass on macOS
- terminal flag PBT: 1024 generated cases / 0 failures
- secret-input state-machine PBT: 1024 generated programs / 0 failures; covers UTF-8, backspace, Ctrl-U/W/D/C, Enter, and CSI/SS3 escape sequences against an independent `String` reference editor
- targeted OpenAI tunnel tests: 23 passed / 0 failed
- full tests: 309 passed / 0 failed / 1 intentional process-boundary E2E ignored
- all-target/all-feature check: pass
- no-default-features all-target check: pass (existing dead-code warnings only)
- Clippy with `-D warnings`: pass
- `cargo fmt -- --check`: pass
- `git diff --check`: pass
- macOS normal dependency graph: 116 -> 114 non-root packages (-2 in Phase 4, -32 from the original baseline)
- macOS `--no-default-features`: 40 -> 40 non-root packages
