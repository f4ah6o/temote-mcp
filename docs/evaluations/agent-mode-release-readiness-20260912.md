# Agent permission mode + developer broker release readiness

Status: implemented and CI-green on `main`; report and documentation landed in the follow-up docs commit.

## Result

- starting SHA: `7bdf092f3e686fbf0706a5eee350b122aa3ec556`
- implementation SHA (code + tests + gateway contract): `6665a5bac6cad2be81e52faa46e44f5545ba7ebf`
- implementation CI: GitHub Actions run `34663730116` for `6665a5b` — **success**
  - `gateway`: success
  - `rust (ubuntu-latest)`: success
  - `rust (macos-latest)`: success
- report/docs commit: the commit containing this report (docs only; verified by the same CI workflow after push)
- OpenCode persistent session/resume: intentionally still deferred (no production session/resume code in this change)

## What changed

### Permission modes and defaults

- `PermissionMode::{Ask, Agent, Yolo}` remains the single source of truth. New local managed sessions and authenticated public `session_start` now default to **Agent** when no mode is requested.
- Explicit `Ask` is still available; explicit local `--yolo` still maps to `Yolo`; public HTTP cannot create, request, or promote `Yolo`.
- Legacy persisted `ask` sessions are not silently migrated: the wire fallback for metadata without `permission_mode` is still `Ask`, and restart/restore paths pass the stored mode through.
- `session permission <id> status|ask|agent|yolo` is supported end to end; `session list` prints the permission mode; `session info`/control views serialize `ask|agent|yolo` with the legacy `yolo` mirror.
- Manual `session restart` now preserves the stored mode for public, named, and legacy local sessions instead of downgrading `Agent` to `Ask`.
- Automatic restart, supervisor restore, and upgrade handoff already carried the mode; explicit tests now cover `Agent` through automatic restart in addition to the existing handoff tests.

### Central approval policy

`src/approvals.rs` adds `ApprovalClass` and one policy function:

| operation class | `ask` | `agent` | `yolo` |
| --- | --- | --- | --- |
| `GitNetwork` (git_fetch/pull/push) | approve | allow | allow |
| `LocalAgent` (`local_agent_run`) | user approval | allow | user approval (existing behavior) |
| `DeveloperTool` (`dev_tool_run`) | approve | allow | allow |
| `Integration` (1Password, kintone, cli-kintone) | approve | allow | allow |
| `LocalStructured` (checkpoint_save, apply_patch, recall_feedback) | approve | allow | allow |
| `CodexAppServer` (codex_status/task_start/task_control) | approve | approve | allow (existing) |
| `HostUnrestricted` (`without_sandbox`) | approve | approve | allow (existing) |

`ensure_local_approval` is the single call path; handlers no longer special-case `mode == Agent`. Agent skips only the Temote-local prompt, after the same structural validation required in `ask`. Tool-specific authorization, sandbox, capability, and secret-isolation rules are unchanged, and `without_sandbox` stays approval-gated in `agent` because it leaves the sandbox.

### `dev_tool_run` developer broker

New MCP tool `dev_tool_run({session_id, tool, operation, args?, cwd?})`:

- Cargo offline: `fmt`, `check`, `clippy`, `test`, `build`.
- Cargo dependency/network: `fetch`, `install`, `update`.
- Vite+ offline: `check`, `lint`, `fmt`/`format`, `test`, `build`, `pack`.
- Vite+ dependency/network: `install`, `add`, `update`, `outdated`, `info`, `rebuild`.
- Explicitly rejected: `vp run|exec|dlx`, `vp upgrade|implode`, all unknown operations, all caller-selected executables or raw argv, and any request key outside `{session_id, tool, operation, args, cwd}`.
- Offline operations run in a dedicated developer sandbox (`SandboxSpec::developer_tool` / `LinuxSandboxPolicy::for_developer_tool`): workspace write plus narrowly scoped tool cache/state roots (`~/.cargo`, `~/.rustup` for Cargo; `~/.vite-plus`, `~/.bun`, `~/.npm`, pnpm/cache roots for Vite+), top-level Git metadata masked read-only, network disabled.
- Dependency/network operations use the same write scope with the explicit network capability (`LinuxNetworkPolicy::LocalAgent` on Linux; Seatbelt `network-outbound` on macOS).
- The child environment reuses the local-agent allowlist/filter (`filtered_environment`); no host-wide passthrough and no secret inheritance.
- cwd resolves through `local_agent::resolve_cwd`, so permitted-root and symlink containment are identical to the agent broker.
- Foreground work returns inline; longer work returns a session-owned `job_id` with the existing bounded output policy.
- Approval flows through `ensure_local_approval` (`ask` prompts; `agent` allows after validation; `yolo` keeps existing behavior).

The gateway `routed-tools.json` contract and `gateway/src/protocol.js` were regenerated/updated together; Rust and Node parity tests pass.

## Negative security tests

Added or strengthened:

- `agent` vs `yolo`: policy matrix test plus `agent_mode_command_remains_sandboxed` (outside write denied) and the existing yolo bypass test.
- ordinary `execute`/`start_command` in Agent stay sandboxed with network disabled (sandbox suite plus the new Agent test; `start_command` shares the same runner).
- public sessions default to `agent`, remain `yolo=false`, and `without_sandbox` stays absent/rejected (HTTP tests now assert `permission_mode=agent`).
- Git invariants: safe-remote grammar, fast-forward-only, current-branch/no-force tests unchanged; `git_fetch` in Agent runs without a console, and the same call in Ask fails closed without one.
- `local_agent_run` cannot become arbitrary executable/argv (existing tests); Agent skips approval only for a prepared, validated run.
- `dev_tool_run` cannot become generic host execution: exact-key schema, operation grammar, argv construction test, unsafe-class rejection test, cwd-outside-roots rejection, and sandboxed fake-tool execution with outside-write denial.
- Vite+ dangerous classes (`run`/`exec`/`dlx`/`upgrade`/`implode`) and unknown Cargo operations are rejected before any child spawn.
- Secrets: dev tools reuse the proven `local_agent::filtered_environment`; existing child-environment, approval-summary, and integration secret tests still pass.
- Integration boundaries: 1Password/kintone authentication, discovery gating, and argument validation remain in place; only the Temote-local prompt is skipped in Agent.

## Local verification (final state)

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo check --no-default-features --all-targets` | pass |
| `cargo test --all-targets --all-features --locked` | 646 bin + 40 lib + 9 e2e passed, 0 failed (2 ignored: pre-existing live test + new ignored Cargo acceptance) |
| `(cd gateway && npm test)` | 60/60 passed |
| `git diff --check` | clean |
| `cargo test --test cli_session_e2e --all-features --locked supervisor_upgrade_ -- --ignored --test-threads=1` | 2 passed |
| `cargo test --test cli_session_e2e --all-features --locked supervisor_session_lifecycle_survives_console_eof_and_records_crash -- --ignored` | 1 passed |
| `cargo test --bin temote-mcp live_cargo_check_in_the_developer_sandbox -- --ignored` | 1 passed (real Cargo `check` inside the developer sandbox) |

Focused suites: approvals policy matrix, supervisor mode defaults/persistence, session-control manual restart preservation, sandbox policy/tests on macOS and Linux, HTTP public defaults, MCP Agent tests, dev_tool classifier/argv/rejection/sandbox tests, gateway contract parity.

## Release-readiness checks

- `dist generate` regenerated `.github/workflows/release.yml` with **no diff**: the committed generated workflow matches `dist-workspace.toml`.
- `dist-workspace.toml` and release workflows were not hand-edited.
- CI's packaging steps (`Verify packaged crate manifest`, `Install from packaged crate source`) and `tests/publishability.rs` pass on the implementation SHA.
- Linux and macOS dependency-boundary checks pass; `cargo check --no-default-features` passes.
- No release tag was created and no crate was published.

## GitHub Actions

- final implementation CI: run `34663730116`, SHA `6665a5b`, conclusion **success** for `gateway`, `rust (ubuntu-latest)`, and `rust (macos-latest)`.
- an earlier macOS run (`34658772084`) failed once in the plain `Test` step on a docs-only commit; the same code passed on the next run and in the final run, and the failure was not reproducible locally (one local flake in `codex_app_server::tests::restart_does_not_start_replacement_until_old_turn_drained` passed on rerun). This is a pre-existing timing-sensitive area, not introduced by this change.
- the docs-only report commit is validated by the same workflow after push (see `gh run list -L 1`).

## Remaining limitations / deferred work

- Live Vite+ acceptance was not added as a CI test because `vp check` requires a Vite+ project setup; the deterministic classifier/argv/rejection/sandbox tests cover the contract. Operators with a Vite+ project can run a live `dev_tool_run` check.
- The ignored `live_cargo_check_in_the_developer_sandbox` acceptance is available for manual verification and was run successfully on this host.
- Cargo dependency operations intentionally write to the host Cargo/Rust cache roots; this is the documented scoped state model, not a new arbitrary HOME write.
- OpenCode persistent session/resume remains deferred per `docs/evaluations/opencode-session-resume-spike-20260912.md`; no session/resume code was added here.
- Pre-existing timing-sensitive Codex app-server tests remain a candidate for future deflaking.

## Git status

- tracked worktree clean after the docs commit; the only untracked entry is the pre-existing `.worktrees/`, which was never modified, staged, or deleted.
- `HEAD == origin/main` after the final push.

## Temote verification

session: `temo`
repository: `/Volumes/DevSSD/Developer/local-mcp`
Verify:
1. read this report
2. `git status --short --branch`
3. `git rev-parse HEAD`
4. `git rev-parse origin/main`
5. `git log --oneline -5`
6. `gh run view 34663730116 --json headSha,conclusion,jobs`
7. focused spot-checks:
   - `cargo test --bin temote-mcp local_approval_policy_matrix_is_explicit`
   - `cargo test --bin temote-mcp new_session_defaults_and_explicit_modes_are_stable`
   - `cargo test --bin temote-mcp manual_restart_preserves_agent_and_ask_permission_modes`
   - `cargo test --bin temote-mcp dev_tool`
   - `cargo test --bin temote-mcp agent_ dev_tool_run`
   - `(cd gateway && npm test)`
8. release readiness: `dist generate` must produce no diff and `.github/workflows/release.yml` must remain unmodified.
