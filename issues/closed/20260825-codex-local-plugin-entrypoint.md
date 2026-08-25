# Codex local plugin entrypoint

Date: 2026-08-25
Status: completed

## Context

Temote MCP already had the two core pieces needed by a local coding agent:

- a local stdio MCP entrypoint: `temote-mcp mcp`
- an Agent Skill under `skills/temote-mcp`

The goal was to make those discoverable through a thin Codex Plugin without moving lifecycle, sandbox, approval, named-root, OAuth, or ingress behavior out of the native `temote-mcp` binary.

## Completed design

The repository root is directly recognizable as a Codex Plugin:

- `.codex-plugin/plugin.json`
- `.mcp.json` -> `temote-mcp mcp`
- existing `skills/temote-mcp/SKILL.md`

For installed binaries, the normal path is binary-owned installation:

```text
temote-mcp codex plugin install
temote-mcp codex plugin uninstall
temote-mcp codex status
temote-mcp codex status --json
temote-mcp codex diagnose
temote-mcp codex diagnose --json
```

The installer follows the local-cache shape used by the current Codex plugin model:

```text
$CODEX_HOME/plugins/cache/debug/temote-mcp/<temote-version>/
  .codex-plugin/plugin.json
  .mcp.json
  .temote-mcp-bin
  skills/temote-mcp/SKILL.md
```

`CODEX_HOME` falls back to `~/.codex`.

The generated `.mcp.json` and `.temote-mcp-bin` both record the canonical exact executable path of the `temote-mcp` binary that performed installation. The plugin therefore does not silently switch to another ambient binary on `PATH`. Re-running install after a Temote upgrade removes stale Temote plugin versions and installs the current version.

The binary generates the installed plugin bundle itself. A separate public `export` command was not added because installation already owns and performs bundle generation; no plugin-side runtime or security implementation is duplicated.

Codex configuration is updated through the exact section:

```toml
[plugins."temote-mcp@debug"]
enabled = true
```

Unrelated configuration is preserved. Uninstall removes the Temote-owned cache root and its enablement section. Removal refuses a symlinked Temote plugin root.

## Diagnostics

`status` / `diagnose` report, including JSON output:

- Codex home and config path
- plugin key and cache path
- enabled / installed state
- installed manifest version and Temote package version
- pinned binary path and current binary path
- exact-binary match state
- generated MCP command and match state
- MCP CLI health for `diagnose`
- concrete problems and aggregate healthy state

`diagnose` probes only the current exact Temote executable with `mcp --help`; it does not execute a mismatched hint from plugin state.

## Security / ownership boundary

The Plugin remains a discovery/operator layer only. It does not own or weaken:

- session lifecycle
- named-root resolution
- filesystem/network sandboxing
- host approval
- yolo policy
- OAuth
- Cloudflare / Tailscale ingress
- OpenAI Secure MCP Tunnel

Local Codex can launch stdio `temote-mcp mcp`. ChatGPT and other remote clients continue to use the applicable HTTP/remote connection profile; the Plugin does not pretend localhost stdio is a ChatGPT transport.

## Acceptance evidence

- [x] repository has a Codex plugin manifest
- [x] repository `.mcp.json` launches `temote-mcp mcp`
- [x] existing Temote Skill is reused rather than duplicated in source
- [x] `temote-mcp codex plugin install` is implemented
- [x] `temote-mcp codex plugin uninstall` is implemented
- [x] `temote-mcp codex status [--json]` is implemented
- [x] `temote-mcp codex diagnose [--json]` is implemented
- [x] installed MCP configuration pins the exact installing Temote binary
- [x] installed state carries a matching binary hint
- [x] stale Temote plugin versions are replaced deterministically on reinstall
- [x] config mutation preserves unrelated settings
- [x] symlinked owned plugin root is rejected before recursive removal
- [x] README / README.ja.md document binary-owned installation as the normal installed-binary path
- [x] `gh skill install ...` remains available for non-Plugin Agent Skill consumers
- [x] `cargo fmt --all -- --check` passes
- [x] `cargo check --all-targets --locked` passes on Ubuntu and macOS
- [x] `cargo check --no-default-features --all-targets --locked` passes on Ubuntu and macOS
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes on Ubuntu and macOS
- [x] `cargo test --all-targets --all-features --locked` passes on Ubuntu and macOS
- [x] CLI session E2E passes on Ubuntu and macOS
- [x] packaged crate manifest verification passes on Ubuntu and macOS
- [x] install from packaged crate source passes on Ubuntu

CI evidence: GitHub Actions run `32860678908` for commit `425ea16e9d904393b782e14b33e3fe59eeaa0d70` completed successfully on both `ubuntu-latest` and `macos-15`.
