---
name: testing-temote-mcp
description: Live-test temote-mcp end-to-end — session lifecycle, stdio MCP JSON-RPC driver, and the owner-only observation/context surfaces. Use when a PR changes MCP tools, delegation paths, or the observation journal and runtime proof is needed.
---

# Testing temote-mcp end to end

## Build

`cargo build --locked` → binary at `target/debug/temote-mcp`. `cargo test` has known
pre-existing failures on Devin VMs (github.com proxy 403s); runtime testing avoids them.

## Live session for MCP tool calls

Session-bound tools (`codex_*`, `context_*`, `evidence_*`) require a *running* session:
`config::load_session` probes a unix socket owned by the supervisor process.

```sh
cd <any-dir>            # cwd becomes the session scope + repository label
./target/debug/temote-mcp start    # prints JSON; capture .session_id
```

`start` auto-spawns `temote-mcp supervisor` (detached, persists across calls — the
supervisor holds the session socket in-process). Non-yolo local sessions get
`permission_mode: agent` → `*_task_start` / `*_task_control` are approval-free.
Cleanup: `./target/debug/temote-mcp session stop <sid>`; `session restart <sid>`
keeps the same session_id (useful for testing state that survives restarts).

## Driving the stdio MCP server

`temote-mcp mcp` speaks line-delimited JSON-RPC 2.0 on stdin/stdout (no MCP client
needed). Each request needs `jsonrpc`, `id`, `method`, `params`; responses arrive
in order, one per line, and the process exits on stdin EOF. `tools/call` params are
`{"name": ..., "arguments": {...}}`; tool errors come back as
`{"error": {"code": -32000, "message": ...}}` on the same id.

A minimal driver (spawn per batch of requests, initialize + tools/call on ids 1,2):
write `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}` then
the `tools/call` line, close stdin, read stdout lines. Example helper used in
testing: `/home/ubuntu/mcp_call.py` (spawn, write lines, print response text).

Put `codex`/`opencode` on PATH first (`export PATH="$HOME/.nvm/versions/node/*/bin:$PATH"`)
— delegation calls resolve the binary by bare name in *this* process's env.

## Observation journal surfaces (O1/O2)

- Journal: `~/.local/state/temote-mcp/observations/obs-<sid>.jsonl`
  (+ `obs-<sid>.meta.json` sidecar only after compaction/write-failure; `locks/`).
  Owner-only: dirs 0700, files 0600.
- Owner CLI (reads files directly; works even for stopped sessions):
  `temote-mcp observation list|get|status <sid>` — `list` supports
  `--kind/--task/--after-revision/--limit/--include-content`; without
  `--include-content` records show content *descriptors* (kind/sha256/total_bytes),
  never bodies.
- MCP tools: `context_resolve` (session_id required; task_id/repository/query/
  limit 1..=64/at_least_revision) and `context_status` (session_id) — both fail for
  stopped sessions.
- Record kinds observed from delegation calls: `instruction` (pre-approval, task
  text as ≤4 KiB preview + sha256), `operation_accepted` + `delivery` (mutating ops),
  `execution_state` (sanitized task views), `verification` (`*_status` probes),
  `reconciliation` (dispatch errors / reconciliation_required views). `evidence`
  needs a backend turn that produced evidence — unreachable without provider creds.
- Dedupe: repeating an identical failed call appends nothing (instruction+error keys
  match). Different operation_ids with identical error text also dedupe via the
  `err:` key's error hash.
- To get *succeeded* records without working provider creds: `codex_task_start`
  persists acceptance and returns a view with `status:"failed"` — accepted/delivery/
  execution_state all still journal. Codex task views contain no instruction text.
- Corrupt lines: hand-append a garbage line to the .jsonl; `list` prints a
  `{"warning","corrupt_lines"}` JSON line and `status` reports degraded=true.

## Devin Secrets Needed

- `TEMOTE_MCP_DEVIN_API_KEY` — only for `devin_cloud_*` live calls; not needed for
  codex/opencode/journal testing.
