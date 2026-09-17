# Changes

## Unreleased

### Added

- Added `temote-mcp activity` for bounded local replay and live observation of session operations, lifecycle changes, approvals, jobs, integrations, and supervisor upgrades. Activity stays on the owner-only local control socket and is not a durable audit log. ([local activity viewer](issues/polished/20260914-local-activity-viewer.md))
- Added authenticated direct HTTP session lifecycle and supervisor upgrade tools with explicit local approval, durable transaction status, and reconnect-after-commit behavior. ([client-safe upgrade reconnect](issues/open/20260908-07-client-safe-upgrade-reconnect.md))
- Added opt-in structured Codex task tools backed by a version-checked local app server, typed control actions, bounded evidence, and persistent reconciliation receipts. ([Codex delegation and app server](issues/open/20260908-08-codex-delegation-dogfood-and-app-server.md))

### Changed

- Supervisor upgrades now coordinate session restore, direct-ingress recovery, endpoint checks, and binary-owned Codex plugin reconciliation before reporting terminal status.
- `agent` permission mode now runs ordinary `execute`/`start_command` in the same sandbox and path containment with the network-enabled development profile, so localhost, LAN, and Internet development traffic works without yolo. `ask` keeps ordinary commands network-disabled, and public HTTP still cannot expose `without_sandbox` or create/promote `yolo`. ([agent development network access](issues/open/20260915-agent-development-network-access.md))

### Fixed

- Codex task cleanup and recovery now preserve accepted operations through process and session transitions without replaying uncertain side effects.

### Deprecated

### Removed

### Security

- Bound activity producers and Codex task runtimes to the exact private session instance so delayed work cannot be attributed to a same-name replacement session.
- Kept activity summaries and durable upgrade/task state bounded and free of command output, prompts, session paths, credentials, and raw errors.

### Migration
