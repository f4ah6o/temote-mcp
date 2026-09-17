# Fix macOS compile regression: stale `SandboxSpec::command` calls in `src/sandbox/policy.rs`

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/done/20260916-agent-network-mode-policy.md` (regression follow-up)
Depends on: none

## Current code and contract

`fddec6b` added the `network_access` argument to `SandboxSpec::command` / `scoped_command`
(`src/sandbox/policy.rs:50`, `:69`) and wired the ordinary-command profile through
`run_with_metadata_roots`. `src/sandbox/policy.rs` is compiled only under
`#[cfg(target_os = "macos")]` (`src/sandbox.rs:19`), so Linux `cargo check`/`cargo test`
never sees the file and stayed green.

Review-time CI `35170905636` / macOS job `105042151118` fails with 5 `E0061`:

```text
src/sandbox/policy.rs:243:24  self::command(cwd, writable_roots)   (SandboxSpec::git)
src/sandbox/policy.rs:268:24  Self::command(cwd, writable_roots)   (SandboxSpec::git_worktree_add)
src/sandbox/policy.rs:404:17  SandboxSpec::command(root.path(), &[file])       (test)
src/sandbox/policy.rs:430:20  SandboxSpec::command(&workspace, &[])            (test)
src/sandbox/policy.rs:458:24  SandboxSpec::command(&cwd, &requested)           (test)
```

## Reproduction

`gh run view 35170905636 --repo f4ah6o/temote-mcp --log-failed` (macOS job) or any macOS
`cargo check --all-targets`. NOT reproducible on Linux because the module is macOS-only.

## The one responsibility to change

Restore callsite/definition agreement with the existing per-caller network policy:

- `SandboxSpec::git` and `SandboxSpec::git_worktree_add` are Git-only profiles whose callers
  (`run_git`, `run_git_worktree_add`) pass `CommandNetworkPolicy::Restricted`; their internal
  `Self::command` call must pass `false` (restricted). Adding `true` to make compilation pass
  is forbidden.
- The three test callsites pass a fixed `false`; they assert root normalization, not network.

## Not changing

- No signature change, no new network selector, no policy semantic change.
- Ordinary `execute`/`start_command` policy (`ask` restricted / `agent` development) untouched.
- macOS profile rendering (`(allow network-outbound)` only for `Development`) untouched.

## Focused tests / verification

- Linux: `cargo check --all-targets`, `cargo test --lib --locked ordinary_command_network_policy`.
- macOS compile gate: `cargo check --target <macos-triple> --all-targets` if a macOS target is
  installed; otherwise CI macOS job is the gate (NOT RUN locally).
- Existing `just sandboxed-check` must stay green.

## Completion condition

All five callsites compile on macOS with Git paths still restricted; Linux focused tests and
`just sandboxed-check` pass; macOS compile recorded as CI result (not claimed from Linux).

## Implementation notes (2026-09-17)

Changes in `src/sandbox/policy.rs`:

- `SandboxSpec::git` (`:243`) and `SandboxSpec::git_worktree_add` (`:268`) pass `false` to the
  internal `Self::command` call, preserving the `CommandNetworkPolicy::Restricted` policy their
  callers (`run_git`, `run_git_worktree_add`) already select. No `true` was added.
- Test callsites `:404`, `:430`, `:458` pass `false`.

Verification:

- `cargo check --no-default-features --target x86_64-apple-darwin --lib --locked`: PASS on this
  Linux host with the macOS target's std installed; proves `src/sandbox/policy.rs` (including the
  two production callsites) type-checks under `target_os = "macos"`.
- `cargo check --no-default-features --target x86_64-apple-darwin --lib --tests --locked`:
  `src/sandbox/policy.rs` test module has no errors; the remaining failures in that combination are
  pre-existing unrelated `doctor.rs`/`activity_cli_e2e` no-default macOS cfg issues.
- `cargo check --target x86_64-apple-darwin --all-targets` (default features): NOT RUN locally —
  cross-compiling `ring`'s C sources needs an Apple toolchain/SDK. The CI macOS job is the gate.
- `just sandboxed-check`: exit 0.

Status: repository-local fix complete; macOS CI re-run is the remaining evidence.
