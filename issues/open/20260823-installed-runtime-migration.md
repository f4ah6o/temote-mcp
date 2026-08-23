# Installed runtime migration command

## Problem

Pre-profile Temote MCP deployments started by the old `just up` wrapper keep runtime ownership in `~/.cache/temote-mcp/up.pids` as two PIDs (`temote-mcp serve` and `cloudflared`). Current `temote-mcp up` owns ingress as a child of one supervisor and uses a locked `up.pid` file instead.

Replacing the installed executable does not replace an already-running old process. A user should not have to manually parse or kill legacy PIDs to move an existing installation to the current lifecycle.

Configuration files (`public.env`, `tunnel-token`) and local session metadata remain compatible and must not be rewritten. Independently running `temote-mcp start <session>` processes must not be killed automatically.

## Implementation

- Add idempotent `temote-mcp migrate` on Unix network builds.
- Make current `temote-mcp up` fail closed with migration guidance while legacy `up.pids` exists, preventing accidental dual supervisors.
- Detect only the known legacy `up.pids` state.
- Read the legacy PID file with `O_NOFOLLOW`, regular-file checks, and a strict byte/grammar bound.
- Before signaling a live PID, verify the recorded process name is exactly the expected legacy owner (`temote-mcp` / `cloudflared`). Fail closed on PID reuse or unexpected processes.
- Gracefully terminate the validated legacy pair and remove only the legacy PID file after the processes are gone; revalidate before any forced termination.
- `--dry-run` reports the action without mutating state.
- Leave `public.env`, `tunnel-token`, session metadata, session sockets, and independently running local sessions untouched.
- No legacy state is an idempotent success.

## Acceptance

- [x] valid legacy `up.pids` parser accepts exactly two positive PIDs
- [x] symlink / special / oversized / malformed legacy state fails closed
- [x] unexpected live process names are never signaled
- [x] stale legacy state is safely removed
- [ ] valid live legacy `serve + cloudflared` state can be stopped
- [x] `--dry-run` does not signal or delete
- [x] current `up.pid` lifecycle remains unchanged
- [x] current `up` refuses to start while legacy `up.pids` remains
- [x] docs describe `install -> migrate -> up`
- [x] normal Rust gates pass

## Evidence

- targeted lifecycle tests: 15 passed / 0 failed
- full `cargo test --all-targets --all-features`: 271 passed / 0 failed / 1 intentional process-boundary E2E ignored
- clippy `-D warnings`, no-default-features check, and `git diff --check`: pass
- isolated stale-state migration removed only the legacy state file
- live host dry-run detected `~/.cache/temote-mcp/up.pids` with the existing legacy `temote-mcp serve` and `cloudflared` PIDs, preserved both processes, and preserved the state file
