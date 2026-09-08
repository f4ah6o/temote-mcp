# Installed runtime and Cloudflare configuration migration

Status: Done
Closed by triage: 2026-09-08

## Resolution

The installed runtime/configuration migration implementation is complete. Repository-local acceptance covers legacy runtime-state parsing and validation, fail-closed process identity checks, stale-state cleanup, dry-run behavior, Cloudflare configuration migration, macOS path migration, owner-only file creation, tunnel-token migration, and current `up` lifecycle compatibility.

The original issue had one remaining unchecked item: destructive live acceptance against a real valid legacy `serve + cloudflared` pair. That requirement is deployment evidence rather than missing implementation.

The original detailed implementation and evidence remain available in repository history before this triage archival change.

## Remaining live evidence

The destructive legacy-runtime migration acceptance is consolidated into `issues/open/20260908-live-acceptance-matrix.md`.
