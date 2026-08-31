# Installed runtime and Cloudflare configuration migration

## Problem

Pre-profile Temote MCP deployments started by the old `just up` wrapper keep runtime ownership in `~/.cache/temote-mcp/up.pids` as two PIDs (`temote-mcp serve` and `cloudflared`). Current `temote-mcp up` owns ingress as a child of one supervisor and uses a locked `up.pid` file instead.

Replacing the installed executable does not replace an already-running old process. A user should not have to manually parse or kill legacy PIDs to move an existing installation to the current lifecycle.

There is also an installed-configuration migration gap. Older checkouts commonly kept Cloudflare settings in checkout-local `.env`, while current commands document `~/.config/temote-mcp/public.env`. In addition, `dirs::config_dir()` resolves to `~/Library/Application Support` on macOS, so the previous loader could disagree with the documented and error-message path even when the user followed the documented `~/.config` layout.

Configuration migration must preserve the existing Cloudflare Access values without copying unrelated checkout secrets. Very old `.env` files can also contain `TEMOTE_MCP_TUNNEL_TOKEN`; current Temote stores that value in the private `~/.config/temote-mcp/tunnel-token` file instead.

Independently running `temote-mcp start <session>` processes must not be killed automatically.

## Implementation

- Keep idempotent `temote-mcp migrate` on Unix network builds.
- Make current `temote-mcp up` fail closed with migration guidance while legacy `up.pids` exists, preventing accidental dual supervisors.
- Detect only the known legacy `up.pids` runtime state.
- Read the legacy PID file with `O_NOFOLLOW`, regular-file checks, and a strict byte/grammar bound.
- Before signaling a live PID, verify the recorded process name is exactly the expected legacy owner (`temote-mcp` / `cloudflared`). Fail closed on PID reuse or unexpected processes.
- Gracefully terminate the validated legacy pair and remove only the legacy PID file after the processes are gone; revalidate before any forced termination.
- Extend `temote-mcp migrate --dry-run` to report compatible Cloudflare configuration migration without writing files.
- Extend `temote-mcp migrate` to migrate compatible Cloudflare configuration when the destination does not already exist.
- Let `temote-mcp up --profile cloudflare` bootstrap the same compatible migration automatically before loading Cloudflare configuration.
- Make the documented macOS default authoritative: `~/.config/temote-mcp/public.env`. Preserve Linux/XDG behavior and honor explicit `TEMOTE_MCP_ENV_FILE`.
- On macOS, recognize the previous accidental `~/Library/Application Support/temote-mcp/public.env` location and copy it to the documented private destination without deleting or overwriting the source.
- Recognize checkout-local `.env` only from a Temote checkout (`Cargo.toml` package `temote-mcp`).
- Parse checkout `.env` with dotenv semantics, require the complete Cloudflare Access configuration, and copy only the supported Cloudflare/runtime keys. Do not copy unrelated variables or gateway secrets.
- If a very old `.env` contains `TEMOTE_MCP_TUNNEL_TOKEN`, migrate it to the private default tunnel-token file. Never overwrite a different existing token.
- Create migrated files with owner-only mode (`0600`) and `create_new`; reject symlink/special/oversized inputs and never overwrite an existing destination.
- Keep `just up` as a thin development wrapper so it does not block the binary's migration path with a preflight `env-check`.
- Leave independently running local sessions untouched.

## Acceptance

- [x] valid legacy `up.pids` parser accepts exactly two positive PIDs
- [x] symlink / special / oversized / malformed legacy runtime state fails closed
- [x] unexpected live process names are never signaled
- [x] stale legacy runtime state is safely removed
- [ ] valid live legacy `serve + cloudflared` state can be stopped
- [x] runtime `--dry-run` does not signal or delete
- [x] current `up.pid` lifecycle remains unchanged
- [x] current `up` refuses to start while legacy `up.pids` remains
- [x] docs describe `install -> migrate -> up`
- [x] macOS default public-env path matches the documented `~/.config/temote-mcp/public.env`
- [x] previous macOS Application Support location is recognized as a migration source but never overwritten or deleted
- [x] checkout-local `.env` migration requires complete Cloudflare Access values
- [x] checkout migration copies only supported keys and excludes unrelated/gateway secrets
- [x] legacy raw Tunnel token is moved to a private token file and a conflicting existing token fails closed
- [x] migrated files are `0600`, created without overwrite, and symlink/special/oversized inputs fail closed
- [x] `temote-mcp migrate --dry-run` reports config migration without writing it
- [x] `temote-mcp up --profile cloudflare` can bootstrap compatible config migration directly
- [x] normal Rust gates pass on the implementation branch

## Evidence

Existing runtime-migration evidence before this follow-up:

- targeted lifecycle tests: 15 passed / 0 failed
- full `cargo test --all-targets --all-features`: 271 passed / 0 failed / 1 intentional process-boundary E2E ignored
- clippy `-D warnings`, no-default-features check, and `git diff --check`: pass
- isolated stale-state migration removed only the legacy state file
- live host dry-run detected `~/.cache/temote-mcp/up.pids` with the existing legacy `temote-mcp serve` and `cloudflared` PIDs, preserved both processes, and preserved the state file

Follow-up configuration-migration evidence:

- GitHub Actions CI run 137: macOS 15 and Ubuntu jobs both passed
- both platforms: rustfmt, all-target check, no-default-features check, Clippy, full tests, and CLI session E2E passed
- Linux: sandbox helper build, live sandbox acceptance, dependency boundary, packaged crate manifest, and install-from-packaged-source passed
- macOS: packaged crate manifest and dependency boundary passed


## Tracking-only status — 2026-09-01

Repository-local migration logic and safety tests are complete. The sole unchecked item is a destructive live-host acceptance proving that an actual legacy `serve + cloudflared` pair can be stopped after process-name verification; it remains open rather than simulating or fabricating live evidence.
