# AGENTS.md

## Purpose

Temote MCP is a Rust MCP server for operating local machines through explicit sessions. Normal sessions are path-scoped, command execution is sandboxed with network disabled, and host/network operations are approval-gated. `--yolo` intentionally removes those Temote MCP boundaries.

## Repository rules

- Product name: **Temote MCP**.
- CLI/package name: `temote-mcp`.
- Environment-variable prefix: `TEMOTE_MCP_`.
- Keep `README.md` and `README.ja.md` short and user-oriented: what it is, installation, first session, Agent Skill installation, and links to deeper docs.
- Put detailed human documentation under `docs/`.
- Put repository-specific agent/development guidance here instead of expanding the README.
- Do not add product-specific web-chat setup instructions. Document standards-based MCP/OAuth behavior in client-neutral terms.
- Preserve upstream attribution to `nakasyou/local-mcp` and the Temote naming credit.

## Safety invariants

Do not weaken these without an explicit issue describing the security model change:

- `session_list` is the only public tool that does not require `session_id`.
- Normal-session filesystem access must remain inside permitted roots, including symlink resolution and command `cwd`.
- Normal `execute` / `start_command` run in the sandbox with network disabled.
- Ordinary sandboxed commands must not gain write access to Git metadata. Use the dedicated Git tools for index/commit/remote operations.
- `git_pull` stays fast-forward-only; `git_push` must not expose force or arbitrary URL/refspec input.
- Public HTTP must not expose `without_sandbox`.
- Host/network operations remain approval-gated in normal sessions.
- `--yolo` may bypass Temote MCP sandbox/path/approval boundaries, but should not silently change unrelated client authorization semantics.
- Secrets must not be written to session metadata, audit logs, approval summaries, or ordinary tool output.
- Child MCP approval summaries should expose argument keys, not secret values.

## Tool behavior that agents should preserve

- Relative paths resolve from the selected session working directory.
- `execute` returns inline when it completes within the foreground timeout; longer work returns a `job_id` for `poll_job` / `stop_job`.
- Background jobs are session-owned and cancelled when the session stops or reaches its lifetime limit.
- 1Password child MCP usage is discover-first: `onepassword_mcp_discover`, then resource/tool calls.
- kintone child MCP usage is status/discover-first and mutating ambiguity remains approval-gated in normal mode.

## Development workflow

Before changing code, inspect the current worktree and relevant issue/document instead of assuming prior state. Keep changes generic rather than adding product/project-specific exceptions.

Run the relevant checks before committing:

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
(cd gateway && npm test)
git diff --check
```

`just check` covers the normal Rust format/test/clippy/diff gates. Run gateway tests when gateway code or shared protocol behavior changes.

## Documentation map

- `docs/usage.md` / `docs/usage.ja.md`: sessions, permissions, tool behavior, safety boundaries.
- `docs/public-http.md` / `docs/public-http.ja.md`: Cloudflare Access/Tunnel public HTTP deployment.
- `docs/integrations.md` / `docs/integrations.ja.md`: 1Password and kintone bridges.
- `docs/gateway.md` / `docs/gateway.ja.md`: Workers/Durable Objects multi-host gateway.
- `docs/development.md`: build, test, release, and contributor details.
- `skills/temote-mcp/SKILL.md`: reusable Agent Skill for operating Temote MCP from a compatible agent.

When behavior changes, update the narrowest relevant document and the skill only if agent-operating guidance also changed.

## Release

Releases use CalVer `YYYY.MM.PATCH` in `Asia/Tokyo` through `f4ah6o/calver-action`. The `latest` tag selects the release candidate; the workflow creates a release-only version commit and immutable CalVer tag rather than merging that version bump back into `main`.
