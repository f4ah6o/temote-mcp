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
