# Detect connected MCP tool-schema drift from the deployed runtime

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Source issue: `issues/closed/20260916-connected-mcp-surface-misses-dev-tool-run.md`
Depends on: Phase 0 runtime refresh

## Goal

Make it possible to tell, without guessing, whether local stdio, authenticated HTTP, gateway, and connected client surfaces expose the same public tool names and input schemas.

## Scope

- add a bounded public contract fingerprint/version to the existing diagnostics path;
- add deterministic tests comparing routed tool names + input schemas across repository-controlled surfaces;
- document that source presence does not imply connected runtime availability;
- add a release/deployment acceptance hook that fails on repository-controlled schema drift.

Do not add a raw tool-list passthrough that exposes private/internal tools. Do not recommend yolo/raw execute as fallback.

## Acceptance

- deterministic parity test covers tool name and exact public input schema;
- operator can identify server/source/gateway contract drift from bounded non-secret output;
- release/deploy gate catches repository-controlled drift;
- focused gateway/Rust contract tests and `just sandboxed-check` pass.

Live connected-client discovery remains a Phase 4 matrix check.

## Implementation notes (2026-09-16)

Current `main` already had two of the three parity links: the Rust snapshot test compared the generated routed contract with `gateway/contract/routed-tools.json`, and the gateway test deep-compared `PUBLIC_TOOLS` plus protocol versions with the same file. What was missing: no fingerprint/version was reported by any running surface, no explicit release acceptance step, and no documentation that repository presence is not connected-runtime availability.

Changes:

- `src/mcp.rs`: moved `routed_gateway_contract` / `strip_gateway_contract_prose` from the test module into production code, added deterministic `canonical_contract_json` (explicit recursive key sort, independent of the `serde_json` `preserve_order` feature) and `public_contract_fingerprint()` (SHA-256 hex, cached).
- The local server now reports the fingerprint from `session_info` (`server_contract_fingerprint`, bounded 64 hex chars) and `server/discover` (`_meta["dev.temote/contractFingerprint"]`).
- `gateway/src/protocol.js`: added `stripContractProse`, canonical serialization, and async WebCrypto `publicContractFingerprint()`; `gateway/src/index.js` `/healthz` now returns `contractFingerprint`.
- Added `gateway/contract/public-tools.fingerprint`, kept in sync by the Rust snapshot test (`TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT=1` regenerates it together with the JSON).
- New tests: `public_contract_fingerprint_matches_checked_in_snapshot` (Rust), `diagnostics_surfaces_report_the_public_contract_fingerprint` (Rust), and `gateway public contract fingerprint matches the checked-in snapshot` (gateway, async).
- `.github/workflows/release.yaml`: explicit `Verify public contract parity` step running both Rust snapshot tests and the gateway suite before publishing.
- `docs/development.md`: new "Connected runtime contract parity" section with the fingerprint sources, comparison rule, and the explicit statement that a merged commit or restarted session does not refresh the connector schema, and that raw `execute`/yolo is not a fallback for a missing tool. `docs/gateway.md` / `docs/gateway.ja.md`: `/healthz` shape updated with the fingerprint and its comparison rule.

Observed behavior:

- Rust and gateway fingerprints match the checked-in file byte for byte across languages: `de1f7c36737f78603dca7ca80a65338956a022ef09b8715cdefd5cf4c3838751`.
- The gateway test also asserts the fingerprint is 64 lowercase hex characters.
- An operator compares the connected runtime's `session_info.server_contract_fingerprint` (or `server/discover` meta) and the gateway `/healthz` `contractFingerprint` against `gateway/contract/public-tools.fingerprint`.
- No raw tool-list passthrough was added; the fingerprint covers only the public routed contract.

Gates:

- `cargo test --bin temote-mcp --locked fingerprint`: PASS (2/2).
- `cargo test --bin temote-mcp --locked routed_gateway_contract_matches_checked_in_snapshot`: PASS.
- `cargo test --bin temote-mcp --locked discover`: PASS (4/4, including HTTP discovery tests).
- `(cd gateway && npm test)`: PASS (71/71, including the new fingerprint test and the updated `/healthz` shape).
- `just sandboxed-check`: exit 0 (lib 112/112, gateway 71/71, clippy clean under the pinned 1.98.0 toolchain). The first clippy run rejected `sort_by` in favor of `sort_by_key`; fixed with no behavior change.
- host/CI-only: NOT RUN (an actual GitHub Actions CI / Allocate Release run, Linux nested sandbox runtime tests, full binary/local Unix-socket integration suite, ignored supervisor/process-boundary E2E); live connected-client discovery remains a Phase 4 matrix check.

## 2026-09-16 completion

All repository-local acceptance items are met. The source issue `issues/closed/20260916-connected-mcp-surface-misses-dev-tool-run.md` is already superseded by this packet and stays closed; live connected-surface verification stays in `issues/open/20260908-live-acceptance-matrix.md`.
