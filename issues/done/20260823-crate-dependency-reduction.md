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

- Evaluate `uuid` and `dotenvy` only where replacement remains simpler than the dependency. `similar` was internalized on 2026-08-23 with the crate retained only as a dev-dependency differential oracle; see the evidence below.
- `clap` was replaced with dependency-free `noargs 0.4.3` on 2026-08-24 while preserving Temote's public CLI surface; see the acceptance and evidence below.
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

### `clap` -> `noargs` migration

- [x] `clap`, `clap_builder`, `clap_derive`, and `clap_lex` are absent from the lockfile and production graph.
- [x] `noargs 0.4.3` is the sole CLI parsing dependency and has no transitive dependencies.
- [x] root commands and defaults remain compatible, including bare `temote-mcp` -> `start`, `doctor`, `start`, `mcp`, `serve`, `up`, `down`, `migrate`, `openai setup`, and `gateway-agent`.
- [x] `--help` / `-h`, `--version` / `-V`, parser error exit code 2, nested OpenAI help, option defaults, repeated scope options, and env-backed options are preserved.
- [x] profile/platform enums are parsed explicitly without derive macros and fail closed on unknown values.
- [x] Linux sandbox helper uses `noargs` only before the `--` command terminator, so child arguments cannot be consumed as helper options.
- [x] Linux helper compiles for `x86_64-unknown-linux-gnu` with `--no-default-features`; the full Linux network cross-check remains host-toolchain-limited by missing `x86_64-linux-gnu-gcc` for `ring`.
- [x] `cargo check --all-targets --all-features` passes.
- [x] `cargo check --all-targets --no-default-features` passes.
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [x] full tests pass.
- [x] `cargo fmt --all -- --check` and `git diff --check` pass.

### `similar` internalization

- [x] `similar` is absent from normal/default and no-default production dependency graphs.
- [x] `similar` remains a dev-dependency and differential oracle only.
- [x] production line diff uses bounded exact LCS for small/medium regions, unique-line patience anchors for large regions, and a bounded replace fallback for unresolved huge regions.
- [x] unified diff preserves three lines of context and missing-final-newline markers for oracle-covered inputs.
- [x] 4,096 generated unique-line edit programs match `similar` exactly for counts and unified-diff rendering.
- [x] 4,096 generated small arbitrary edit programs match `similar` added/removed counts.
- [x] a large 4,000-line fully distinct case proves the bounded fallback path without quadratic allocation.
- [x] `cargo check --all-targets --all-features` passes.
- [x] `cargo check --all-targets --no-default-features` passes.
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [x] full tests pass.
- [x] `cargo fmt -- --check` and `git diff --check` pass.

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

## `similar` internalization evidence

Implemented on 2026-08-23 against `main` after commit `740a595`.

- usage remains isolated behind `src/mcp.rs::render_diff`, now delegated to `src/line_diff.rs`
- `similar 2.7.0` has no transitive dependencies, but it is removed from `[dependencies]` and retained under `[dev-dependencies]` only as a differential oracle
- production implementation uses exact line LCS when the DP matrix is at most 1,000,000 cells; larger regions are split on order-preserving lines that are unique on both sides, with a bounded replace fallback when no safe anchors remain
- three-line unified-diff context and missing-final-newline markers are preserved for the oracle-equivalence domain
- differential PBT: 4,096 generated unique-line edit programs match `similar` exactly for added/removed counts and rendered unified diff
- differential PBT: 4,096 generated small arbitrary programs match `similar` added/removed counts even when repeated lines make edit ordering non-unique
- large-input invariant: 4,000 fully distinct old/new lines exercise the bounded fallback without quadratic allocation
- targeted internal diff tests: 5 passed / 0 failed
- full tests: 314 passed / 0 failed / 1 intentional process-boundary E2E ignored
- all-target/all-feature check: pass
- no-default-features all-target check: pass (existing dead-code warnings only)
- Clippy with `-D warnings`: pass
- `cargo fmt -- --check`: pass
- `git diff --check`: pass
- macOS normal dependency graph: 114 -> 113 non-root packages; `similar` normal refs = 0, dev refs = 1
- macOS `--no-default-features`: 40 -> 39 non-root packages
- `cargo llvm-lines --bin temote-mcp --release --all-features`: 851,840 -> 829,303 LLVM IR lines (-22,537, about -2.65%) and 19,278 -> 18,972 copies (-306)
- release llvm-lines contains no `similar::` functions; internal `temote_mcp::line_diff` accounts for about 2,771 LLVM IR lines
## `clap` -> `noargs` migration evidence

Implemented on 2026-08-24 against `main` after `bd19696`.

- replaced `clap = { version = "4", features = ["derive", "env"] }` with `noargs = "0.4.3"`; `noargs` reports zero dependencies and no macros/implicit I/O
- moved the public CLI definition into `src/cli.rs` using imperative command/option/flag parsing; `Profile` and gateway `Platform` now use explicit `FromStr` implementations
- migrated the Linux sandbox helper parser as well; it splits at the first exact `--` before passing helper arguments to `noargs`, preserving arbitrary child command flags after the terminator
- added 10 CLI compatibility tests for bare-start default, optional session ID + `--yolo`, flag-before-positional ordering, unknown-option fail-closed handling, dash-prefixed session IDs after `--`, root help/version, unknown command/profile rejection, network defaults, nested OpenAI help, repeated OpenAI scopes, and gateway-agent required options/platform parsing
- real binary checks: `--version` and `-V` print the package version; root and nested help render; unknown command exits with status 2; every root/nested help scope has the exact same long-option set as the previous `bd19696` clap implementation, and subcommand `--version` rejection remains unchanged
- secret-backed gateway options expose only environment variable names in noargs help metadata, not environment values
- Linux helper `--no-default-features` test target compiles for `x86_64-unknown-linux-gnu`; a full network cross-build is blocked locally before Temote code by the missing `x86_64-linux-gnu-gcc` required by `ring`
- full tests: 324 passed / 0 failed / 1 intentional process-boundary E2E ignored
- all-target/all-feature check: pass
- no-default-features all-target check: pass (existing dead-code warnings only)
- Clippy with `-D warnings`: pass
- `cargo fmt --all -- --check`: pass
- `git diff --check`: pass
- macOS normal dependency graph: 113 -> 101 non-root packages (-12 in this migration, -45 from the original 146-package baseline)
- macOS `--no-default-features`: 39 -> 27 non-root packages (-12 in this migration, -16 from the original 43-package baseline)
- `cargo llvm-lines --bin temote-mcp --release --all-features`: 829,303 -> 808,052 LLVM IR lines (-21,251, about -2.56%) and 18,972 -> 18,273 copies (-699, about -3.68%)
- release llvm-lines contains no `clap` symbols; `noargs` plus Temote's explicit CLI parser replaces the derive/builder codegen while keeping the public command surface explicit


## Completed — 2026-09-01

All repository-local acceptance criteria are complete. The campaign remains intentionally bounded: later removal of mature dependencies is not an open requirement without a new architectural or measured cost reason. Current all-target/all-feature check and full tests remain green after subsequent lifecycle work.
