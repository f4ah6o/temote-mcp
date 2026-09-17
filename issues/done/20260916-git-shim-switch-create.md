# Local-agent Git shim slice 1: switch and create branches

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: Phase 0 OpenCode canary

## Goal

Inside `local_agent_run`, make ordinary `git switch <existing>` and `git switch -c <new>` use validated parent-side branch operations without exposing `.git` as writable.

## Scope

- introduce the minimal Temote-owned `git` shim/broker plumbing needed for only these switch/create forms;
- reuse existing structured branch-create/switch validation from current `main`;
- preserve dirty/untracked work and fail closed on conflicts;
- pass all unsupported/unsafe forms to a fixed rejection, not host git mutation.

Do not implement add/commit/worktree/network in this packet.

## Acceptance

- `git switch main`, existing branch, and `-c` new branch work inside agent mode;
- conflicting local changes are preserved and operation fails closed;
- `.git` remains protected from direct agent writes;
- option/config/alias injection is rejected;
- Linux/macOS deterministic tests plus `just sandboxed-check` pass.

## Implementation notes (2026-09-16)

Current `main` had no shim/broker plumbing; only the structured `git_switch` / `git_branch_create` MCP tools existed.

Architecture as implemented:

- The shim is the `temote-mcp` binary itself, exposed to the agent as a private `<agent-state>/bin/git` symlink. `src/main.rs` dispatches on `argv[0]` basename `git` when `TEMOTE_MCP_GIT_BROKER_DIR` is set, and a hidden `git-shim` entry point exists for deterministic tests.
- Transport is a private request/response directory under the agent state root (`requests/`, `responses/`, 0700), not a Unix socket: the Linux local-agent seccomp profile denies `socket(AF_UNIX, ...)` and the macOS `sun_path` limit is too short for the state root. The shim stages a bounded JSON request and renames it into `requests/`; the broker polls, validates, executes, and writes `responses/<id>.json`. Requests are bounded to 64 KiB, responses to 8 MiB, and the shim times out after 10 s with the fixed rejection.
- `src/agent_git.rs` classifies only `switch <branch>` and `switch -c|--create <branch>`; every other argv (global options, `-f`/`--force`, `-C`, `-c core.hooksPath=...`, `--config-env`, extra args, other subcommands, option-like branches) is rejected before any Git process runs. The broker revalidates the shim-provided cwd against the prepared session roots and reuses `validate_git_branch_name`, `ensure_local_branch_exists/absent`, `resolve_git_base_commit`, `build_git_branch_create_command`, and `build_git_switch_command` from `src/mcp.rs` (made `pub(crate)`).
- Execution is host-side through `sandbox::run_git` with validated Git metadata roots (or `run_unrestricted` in yolo), so `.git` remains read-only for direct agent writes: no `.git`/`.agents`/`.codex` path is added to any writable root.
- `src/local_agent.rs` creates the private `bin/` and broker directories, prepends it to `PATH`, sets the broker environment variable, carries the session into `PreparedRun`, starts the broker for the duration of `run`, and re-exposes the canonical shim target through `read_only_files` when it is not already inside a visible root.

Tests:

- `cargo test --bin temote-mcp --locked agent_git`: PASS (7/7) — classifier allow/deny matrix, cwd containment rejection, unsupported-argv rejection before touching a repository, real temporary-repository `switch -c` -> `switch main` -> dirty-worktree conflict preservation, and a shim/broker file-queue round trip.
- `cargo test --lib --locked linux_local_agent_git_shim_executes_from_state_and_uses_the_private_queue`: PASS on this host (host-only `sandbox::linux_tests`) — a `git` symlink in the private state bin pointing at a host read-only file executes inside the real local-agent sandbox and reads/writes the private queue; this is the transport/seccomp proof for the chosen design.
- Real-binary smoke: invoking the built binary through a `git` symlink and through `git-shim` with no broker prints the fixed message and exits 128.
- `just sandboxed-check`: exit 0 (lib 113/113, `agent_git` 7/7, gateway 71/71, clippy/no-default/diff clean).
- host/CI-only: NOT RUN (a real `local_agent_run` with an installed Codex/OpenCode invoking the shim end-to-end, macOS host execution, actual CI). A live matrix row was added for the rebuilt-runtime end-to-end check.

## 2026-09-16 completion

All repository-local acceptance items are met. The end-to-end agent-driven `git switch` flow on a rebuilt runtime is a Phase 4 live matrix row; the parent umbrella stays open for the remaining Git shim packets.
