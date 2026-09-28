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

## Supervisor upgrade / handoff testing

The `upgrade` flow applies the *installed* binary to a *running* supervisor —
never point it at the shared repo binary or real state. Mirror
`tests/cli_session_e2e.rs`:

- Private binary dir: copy `target/debug/temote-mcp` AND
  `target/debug/temote-linux-sandbox` side by side (helper generation is
  classified next to the installed locator), chmod 700.
- Run every spawn under a *clean* environment like `isolate_process()`'s
  `.env_clear()` — inherited variables defeat the isolation, most notably an
  ambient `TEMOTE_MCP_INTERNAL_INSTALLED_LOCATOR`, which would make `upgrade`
  re-exec a shared/real binary instead of the private copy. Wrap each
  invocation as `env -i PATH="$PATH" HOME=<state> XDG_STATE_HOME=<state>
  XDG_CACHE_HOME=<state>/cache XDG_CONFIG_HOME=<state>/config
  XDG_RUNTIME_DIR=<state>/xdg-runtime TMPDIR=<state>/tmp
  CODEX_HOME=<state>/codex TEMOTE_MCP_RUNTIME_DIR=<state>/runtime
  TEMOTE_MCP_SOCKET_NAMESPACE=<ns> <command>` (all state dirs 0700; `<ns>` is
  1-12 ASCII alnum/`-`/`_`, identical on supervisor AND every CLI call —
  the supervisor socket lives at `/tmp/tmcp-<uid>-<ns>/supervisor.sock`).
- Spawn `temote-mcp supervisor` with `TEMOTE_MCP_ROOTS="src=$project"` (or
  the JSON-object form `TEMOTE_MCP_ROOTS='{"src":"/absolute/project"}'` —
  each value must be a quoted string) in the background with null stdio,
  and SIGINT/kill the child when done — a plain foreground child like the
  e2e helper's is enough; do not require `setsid` (absent on macOS). Wait
  for `session list` to exit 0, then `session start --path src/<subdir>
  <id>` (logical root+relative path).
- "Installed" binary = `current_exe` unless
  `TEMOTE_MCP_INTERNAL_INSTALLED_LOCATOR=<path>` overrides it — use a private
  copy so `upgrade` never re-execs the repo build. Set this variable only on
  the single `upgrade` invocation when a test deliberately redirects the
  locator (e.g. to a helperless bundle, which exercises the
  helper-generation gate rejecting `upgrade --force`); the
  supervisor/session env must never carry it.
- `handoff_required = --force || source_version != target_version`: to
  exercise non-force version-diff paths, patch the version bytes in a binary
  copy (`python3 -c` replace b"X.Y.Z" with a same-length version — verify via
  `<copy> supervisor --capabilities`). macOS only: the byte patch invalidates
  the Mach-O code signature and the kernel SIGKILLs the patched copy — re-sign
  it ad-hoc and verify before executing:
  `codesign --force --sign - <copy> && codesign --verify <copy>`.
- Supervisor PID / boot_generation: unix-socket ping — connect to
  `/tmp/tmcp-<uid>-<ns>/supervisor.sock`, send `{"command":"ping"}`, read
  `result.pid` / `result.boot_generation` (same PID + new boot_generation =
  same-PID handoff).
- Unrestorable-session fixture: `session start --path src/victim <id>` then
  `rm -rf <project>/victim` → the workspace-resolve check blocks it.
- Keep EVERY other CLI invocation's env identical — session restart contexts
  are captured from the start env and mismatches turn healthy sessions
  blocked (the deliberate per-invocation locator override above is the only
  exception).

## Devin Secrets Needed

- `TEMOTE_MCP_DEVIN_API_KEY` — only for `devin_cloud_*` live calls; not needed for
  codex/opencode/journal testing.
