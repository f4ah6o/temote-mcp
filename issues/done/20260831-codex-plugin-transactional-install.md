# Transactional Codex plugin installation and removal

Date: 2026-08-31
Status: done
Branch: main

## Background

The binary-owned Codex Plugin entrypoint is now the default local installation path:

```text
temote-mcp codex plugin install
temote-mcp codex plugin uninstall
```

The installer correctly pins the exact Temote binary, preserves unrelated config text in the normal case, removes stale versions, and refuses a symlinked owned plugin root. Its multi-file update is not yet transactional.

## Evidence

Observed in `src/codex.rs` on `main` at `1ecc1bd`:

- `install_at` calls `remove_owned_plugin_root` before creating and validating the replacement bundle.
- Plugin manifest, MCP configuration, skill, and binary hint are written directly with `fs::write`.
- `set_config_enabled` rewrites `$CODEX_HOME/config.toml` directly with `fs::write`.
- `uninstall_at` removes the plugin directory before disabling its config entry.
- There is no installer lock, staging directory, atomic rename, rollback, or failure-injection test.
- Current tests cover the happy path, stale-version cleanup, unrelated-config preservation, and root symlink refusal, but not interruption or concurrent mutation.

## Problem

A process interruption, disk-full condition, permission error, or concurrent install/uninstall can leave Codex in a worse state than before the command:

- reinstall can delete the last working bundle before the new bundle is complete;
- a partial bundle can coexist with an enabled config entry;
- uninstall can leave an enabled entry pointing to a deleted bundle;
- direct config rewrite can truncate or lose a concurrent unrelated Codex config update;
- retry behavior is not defined for staging remnants or partially completed operations.

This is especially likely during upgrades because each new Temote CalVer installs into a new versioned cache directory and cleans older versions.

## Goals

1. Preserve the last known-good plugin until a complete replacement is ready.
2. Make config changes atomic and concurrency-safe.
3. Make install/uninstall retryable after failure.
4. Preserve exact-binary pinning and existing symlink defenses.
5. Report recoverable partial state clearly through `status` / `diagnose`.

## Proposed transaction model

### Install

1. Acquire a Temote-owned installer lock under `$CODEX_HOME`.
2. Read and validate the existing config and plugin state.
3. Build the entire new plugin bundle in a unique sibling staging directory.
4. Validate the staged manifest, MCP command, skill, binary hint, file types, and expected exact binary path.
5. Atomically rename the complete staged directory into the final version path.
6. Atomically enable the exact plugin config section.
7. Remove stale Temote-owned versions only after the new bundle and config are committed.
8. On failure, remove only Temote-owned staging state and retain the previous working version.

### Uninstall

1. Acquire the same installer lock.
2. Atomically disable/remove the Temote plugin config section first.
3. Remove only the verified Temote-owned plugin root.
4. Treat a disabled but still-present bundle as safe, diagnosable cleanup debt if removal fails.

### Atomic config update

- Write a sibling temporary file with restrictive permissions.
- Preserve the existing config's relevant permission bits.
- Flush and atomically rename on the same filesystem.
- Define and test policy for a symlinked `config.toml` instead of accidentally replacing or following it.
- Detect a concurrent modification between read and commit and retry or fail without overwriting it.
- Keep unrelated bytes unchanged except for the owned plugin section and minimal surrounding newline normalization.

## Non-goals

- Managing third-party plugin cache entries.
- Reformatting the whole Codex TOML file.
- Starting or stopping Temote sessions.
- Changing the plugin key, cache layout, or exact-binary pinning contract.

## Acceptance criteria

- [ ] A failed reinstall never removes the previously valid Temote plugin.
- [ ] Codex config is never exposed as a partially written file.
- [ ] A failed uninstall does not leave an enabled entry pointing to an already removed bundle.
- [ ] Concurrent install/install and install/uninstall operations serialize or fail safely.
- [ ] Concurrent unrelated config mutation is detected and is not silently overwritten.
- [ ] Staging paths are unique, owner-private, and removed after success or recoverable failure.
- [ ] Only a fully validated bundle is moved into the final version path.
- [ ] Stale versions are removed only after the replacement is usable.
- [ ] Regular-file, missing-file, symlink, permission-error, and malformed-config policies are explicit and tested.
- [ ] `status --json` / `diagnose --json` identify recoverable staging, dangling-config, and stale-version states without exposing secrets.
- [ ] Existing exact-binary pinning and symlinked plugin-root refusal remain intact.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo test --all-targets --all-features --locked` passes.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo check --no-default-features --all-targets --locked` passes.
- [ ] `git diff --check` passes.

## Test strategy

Add deterministic failure injection around each transaction boundary:

- after lock acquisition;
- after staging-directory creation;
- after each staged file write;
- after staged validation;
- after final bundle rename;
- before and during config commit;
- before stale-version cleanup;
- during uninstall config removal and bundle cleanup.

For every injected failure, assert the complete postcondition rather than only the returned error:

- prior valid plugin remains usable, or the plugin is safely disabled;
- config parses and preserves unrelated settings;
- no enabled config points to an incomplete bundle;
- retry converges to the requested state;
- no unowned path is removed.


## Implementation evidence

Completed on 2026-08-31.

- Install/uninstall operations use one same-user advisory lock under `CODEX_HOME`; concurrent operations fail safely and crash releases the OS lock.
- A complete owner-private sibling bundle is written, flushed, structurally validated, and committed with Linux `renameat2(RENAME_EXCHANGE)` or macOS `renamex_np(RENAME_SWAP)`. Reinstall keeps the previous bundle until config commit succeeds and rolls back on every injected pre-commit failure.
- Codex config reads reject symlinks, special files, non-UTF-8/oversized inputs, duplicate or malformed owned sections, and preserve unrelated bytes and permission bits. Updates use a flushed private sibling file, concurrent-content comparison, and atomic rename.
- Uninstall atomically disables config before bundle removal. A removal failure leaves a disabled diagnosable bundle instead of an enabled dangling entry.
- `status --json` and `diagnose --json` expose `transaction_artifacts`, `stale_versions`, `dangling_config`, and `disabled_bundle`.
- Deterministic tests inject failure after lock acquisition, staging creation, every staged file write, staged validation, bundle swap, and config temporary-file sync. They verify rollback, staging cleanup, retry convergence, config concurrency protection, and uninstall ordering.
- Transaction-focused tests passed 12/12. The full all-target/all-feature suite passed 318 binary tests plus 33 library tests and the publishability test; one documented process-boundary E2E remains intentionally ignored in the normal suite.
- macOS live acceptance passed install, healthy JSON status, same-version reinstall, uninstall, and cleanup in an isolated `CODEX_HOME`.
- Clippy with `-D warnings`, local and Linux-target no-default-feature checks, rustfmt, and `git diff --check` passed. The full Linux all-feature target remains delegated to CI because the macOS host lacks `x86_64-linux-gnu-gcc`.
