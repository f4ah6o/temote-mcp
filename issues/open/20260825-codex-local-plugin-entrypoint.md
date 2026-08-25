# Codex local plugin entrypoint

Date: 2026-08-25

## Context

Temote MCP already has the two core pieces needed by a local coding agent:

- a local stdio MCP entrypoint: `temote-mcp mcp`
- an Agent Skill under `skills/temote-mcp`

Today those are discovered and installed separately. The desired product entrypoint is a Temote plugin that bundles discovery of the MCP server and the existing Skill without moving lifecycle, sandbox, approval, or session logic out of the native `temote-mcp` binary.

OpenAI's current `tunnel-client` Codex plugin uses the same architectural boundary: the plugin is thin and the native binary remains the owner of protocol/runtime behavior.

## Goal

Make this repository usable as a local Codex plugin so that installing/enabling the plugin exposes both:

- the Temote MCP stdio server
- the existing Temote Skill

The plugin must remain a discovery/operator layer, not a second implementation of Temote.

## Phase 1: repository plugin entrypoint

- add `.codex-plugin/plugin.json`
- add `.mcp.json` pointing at `temote-mcp mcp`
- reuse `skills/temote-mcp` from the repository root; do not duplicate the Skill
- document the plugin-first local Codex path in README / README.ja.md
- keep `gh skill install ...` as a portable fallback for agents that do not consume Codex plugins
- require an installed `temote-mcp` binary on `PATH`; do not auto-download or execute an untrusted binary

## Phase 2: binary-owned Codex integration

Follow the `tunnel-client` pattern once the local manifest has proven useful:

```text
temote-mcp codex plugin install
temote-mcp codex plugin uninstall
temote-mcp codex status
temote-mcp codex diagnose --json
```

Requirements:

- the native binary owns plugin install/export/status/diagnostics
- installed plugin state records the exact selected Temote binary rather than silently switching to an unrelated ambient binary
- upgrades are deterministic and version-aware
- diagnostics report plugin source, enabled state, resolved binary, binary version, and MCP launch health
- Plugin code never owns session lifecycle, named-root resolution, sandbox policy, approval, OAuth, or ingress logic

## ChatGPT boundary

The local Codex plugin may launch stdio `temote-mcp mcp` directly.

ChatGPT must continue to use one of Temote's remote connection profiles where required. In particular, OpenAI Secure MCP Tunnel remains the private/local-machine path for supported OpenAI surfaces. Do not make the local plugin pretend that ChatGPT can connect directly to localhost stdio.

## Acceptance

Phase 1 is complete when:

- Codex can recognize the checkout as a plugin manifest
- plugin MCP configuration launches `temote-mcp mcp`
- bundled skill discovery points to the existing `skills/` tree
- no duplicate Temote runtime/security implementation is introduced
- README distinguishes plugin-first Codex usage from remote ChatGPT/HTTP profiles

Phase 2 remains open until binary-owned install/status/diagnose is implemented and validated.
