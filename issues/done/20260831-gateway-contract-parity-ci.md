# Gateway protocol parity and CI gates

Date: 2026-08-31
Status: done
Branch: main

## Background

Temote MCP has two implementations of the public gateway-facing MCP contract:

- Rust host runtime: `src/mcp.rs::tools(public = true, managed_sessions = false)`
- Cloudflare Worker gateway: `gateway/src/protocol.js::PUBLIC_TOOLS`

The Worker currently has 44 passing protocol tests, but those tests are not part of the repository's default or hosted CI gates.

## Evidence

Observed on `main` at `1ecc1bd`:

- `.github/workflows/ci.yaml` runs Rust format/check/clippy/tests on Ubuntu and macOS, but does not set up Node or run `gateway/npm test`.
- `just check` runs only Rust format/test/clippy plus `git diff --check`.
- `AGENTS.md` and `docs/development.md` require `(cd gateway && npm test)` only as a separate manual command.
- `gateway/src/protocol.js` manually repeats all 25 routed public tool definitions, including names, annotations, required fields, limits, and descriptions.
- Rust and JavaScript separately repeat legacy/modern MCP protocol versions.
- Gateway version `2026.8.0` is repeated in `gateway/package.json`, `gateway/wrangler.toml`, `gateway/src/index.js`, and `gateway/src/protocol.js`, while the CalVer release workflow updates only `Cargo.toml` and `Cargo.lock`.
- A local `cd gateway && npm test` run passes 44/44 tests in about 0.18 seconds, so adding the unit suite to normal gates has negligible runtime cost.

## Problem

A gateway-only regression can reach `main` while GitHub Actions remains green. More importantly, independent Rust and JavaScript unit tests cannot detect a contract change made on only one side.

Possible drift includes:

- a tool accepted by the gateway but missing or rejected by the host;
- different required fields, bounds, defaults, or annotations;
- accidental exposure of `without_sandbox`;
- accidental advertisement of supervisor-only `session_start` / `session_stop` through the gateway;
- different MCP protocol-version behavior;
- stale `serverInfo.version` after a release.

This boundary is security-sensitive because the Worker advertises and filters calls before the Rust host performs the authoritative operation.

## Goals

1. Make the routed public MCP contract machine-checkable across Rust and Worker implementations.
2. Make Worker tests a required local and GitHub Actions gate.
3. Remove repeated, manually synchronized gateway version literals.
4. Preserve the existing gateway topology and security policy.

## Proposed design

### 1. Establish a canonical routed-tool contract

Use the Rust public, non-supervisor tool surface as the source contract:

`tools(public = true, managed_sessions = false)`

Expose it to tests as deterministic normalized JSON, either through a checked-in generated artifact or a repository-only export helper.

The gateway must consume that artifact or compare its exported `PUBLIC_TOOLS` against it in a parity test. The comparison must include at least:

- tool names and ordering policy;
- annotations;
- input schema properties;
- required fields;
- `additionalProperties`;
- array/string bounds and defaults.

Gateway-specific wording may remain as an explicit small overlay, but input schemas and security annotations must not silently diverge.

The contract test must explicitly prove that:

- `without_sandbox` is absent;
- `session_start` and `session_stop` are absent from the routed gateway surface;
- every advertised gateway tool can be dispatched by the host;
- every host tool intended for routed public use is advertised by the gateway.

### 2. Add mandatory gateway gates

- Add `cd gateway && npm test` to `just check`.
- Add a Node 20+ gateway job or step to `.github/workflows/ci.yaml`.
- Run the cross-runtime contract parity check in GitHub Actions.
- Keep the gateway job independent from the Rust OS matrix unless an OS-specific check is required.
- If Wrangler dry-run becomes mandatory, pin Wrangler with a lockfile and use `npm ci`; do not depend on an unpinned network-time `npx` install.

### 3. Unify protocol and release identity

- Keep legacy and modern MCP protocol versions in one machine-checked contract or add exact Rust/Worker parity assertions.
- Make gateway server version come from one deploy/build input.
- Remove repeated source fallbacks that can remain at an older CalVer.
- Define whether the gateway reports the Temote CalVer, a deployment revision, or both, and test the selected shape.
- Update the release/deploy workflow so a new release cannot silently retain the previous gateway identity.

## Non-goals

- Deploying the Worker to a live Cloudflare account.
- Changing gateway authentication, generation, lease, or request-routing semantics.
- Exposing `without_sandbox`, `session_start`, or `session_stop` through the gateway.
- Requiring the Worker implementation to share Rust runtime code.

## Acceptance criteria

- [ ] `just check` runs the gateway unit suite.
- [ ] GitHub Actions fails when any gateway test fails.
- [ ] GitHub Actions fails when a routed tool name, annotation, or input schema differs from the canonical Rust contract.
- [ ] The parity check covers all currently advertised 25 gateway tools without relying on a count-only assertion.
- [ ] `without_sandbox`, `session_start`, and `session_stop` remain absent from the gateway contract.
- [ ] MCP protocol versions cannot diverge between Rust and Worker without a failing test.
- [ ] Gateway version identity has one authoritative input and no stale repeated fallback literals.
- [ ] Existing 44 Worker tests pass.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo test --all-targets --all-features --locked` passes.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo check --no-default-features --all-targets --locked` passes.
- [ ] `git diff --check` passes.

## Test additions

- Contract parity test with a useful structural diff on failure.
- Negative fixture that adds or changes one Worker schema field and proves the parity gate fails.
- Negative fixture that attempts to add `without_sandbox`.
- Protocol-version mismatch fixture.
- Version injection/fallback test.
- Existing Worker protocol suite in CI.

## Risks and constraints

- Tool descriptions have some intentional gateway-specific wording. Keep those overrides explicit and narrow instead of weakening schema comparison.
- Generated artifacts must be deterministic and checked for staleness.
- The parity mechanism must not require network access.
- The Rust host remains the final authorization and validation boundary; contract parity is defense in depth, not a replacement for host validation.


## Implementation evidence

Completed on 2026-08-31.

- Rust emits a deterministic normalized snapshot for `tools(public = true, managed_sessions = false)`; Rust tests fail when the checked-in snapshot is stale.
- Worker tests compare all routed tool names, annotations, schemas, required fields, bounds, defaults, and protocol versions against that snapshot. Prose-only title/description differences remain an explicit normalization boundary.
- Negative fixtures prove schema drift, protocol drift, and `without_sandbox` exposure fail the parity assertion.
- `just check`, GitHub Actions, and release validation run the gateway suite.
- Gateway `serverInfo.version` now uses Cloudflare's `GATEWAY_DEPLOYMENT` version metadata binding. The private npm package uses `0.0.0` and no CalVer fallback remains in Worker source.
- Rust snapshot test passed; Node gateway suite passed 46/46; `git diff --check` passed.
