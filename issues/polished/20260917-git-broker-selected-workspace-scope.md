# Git broker mutation scope must be the selected workspace, not all session roots

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: none

## Current code and contract

`local_agent::run` passes `prepared.session_roots` (every canonical session root) to
`GitBroker::start` (`src/local_agent.rs:760`), and the broker validates each request `cwd` against
all of those roots (`validate_request_cwd`, `src/agent_git.rs:155`). `run_git_command` then executes
Git with `session.permitted_directories` as the writable scope (`src/agent_git.rs:237`). `add`
paths are validated against the same broad root set (`resolve_shim_add_path`, `:133`), and the
staged-path check (`mcp::ensure_staged_paths_are_permitted`) uses the full session too.

The selected `local_agent_run` cwd is already the agent sandbox write root
(`src/local_agent.rs:766`); the broker scope must match it.

## Reproduction

- Session permits root A and sibling root B; run with `cwd = A`; shim request with `cwd = B` and
  `switch -c` currently succeeds, mutating B's Git metadata.
- `add` of a path that resolves into B through a symlink is currently accepted when B is a session
  root.

## The one responsibility to change

Make the canonical selected workspace the fixed operation scope:

- Broker start captures `workspace` (canonical `prepared.cwd`), the workspace's
  `git_worktree_root`, and its `git_metadata_roots` as the pinned repository identity.
- A request is accepted only when its canonical `cwd` is the workspace or a descendant and when its
  re-resolved worktree root and metadata roots equal the pinned identity. A different root, a
  symlinked target, a nested repository, or a swapped linked worktree fails closed.
- Mutation commands run with writable scope `[workspace]` only; a workspace that is not itself a Git
  worktree root, or whose metadata is not contained by it, is rejected instead of widening to a
  parent repository.
- `add` paths and the staged-path check are validated against `[workspace]`, never
  `session.permitted_directories`.
- No parent-repository traversal to regain scope.

## Not changing

- Request `cwd` is still reported and used to resolve `add` paths relative to the agent's cwd.
- Structured `git_*` MCP tools keep their existing session-root behavior.
- Read-only commands (Packet F) are not routed through this mutation path.

## Focused tests

- root A selected, session permits A and B: mutation request for B is rejected and B's HEAD,
  branch, index, working tree and Git metadata are unchanged.
- symlink cwd pointing from A to B rejected; nested repository under A rejected.
- linked worktree substituted for the selected workspace rejected.
- `add` path escaping A through a symlink rejected.
- normal `switch -c` / `add` / `commit` inside A still succeeds.

## Host / CI / provider verification

- Linux host/CI focused tests; macOS host execution NOT RUN unless a macOS runner is used.

## Completion condition

No mutation can target anything but the selected workspace's pinned repository; focused tests and
`just sandboxed-check` pass.

## Implementation notes (2026-09-17)

Changes in `src/agent_git.rs`:

- `BrokerScope::for_workspace` pins the canonical selected workspace and, when the workspace is
  itself a Git worktree root, its `git_worktree_root` + `git_metadata_roots` identity.
- `BrokerScope::resolve_cwd` accepts only a request `cwd` that canonicalizes to the workspace or a
  descendant and whose re-resolved repository identity equals the pinned identity.
- `add` paths are validated against `[workspace]`; staged-path validation runs with a narrowed
  session whose permitted directories are `[workspace]`; Git mutations run with `[worktree_root]`
  as the only writable root. A workspace that is not its own worktree root (for example a
  repository subdirectory) is rejected instead of widening to a parent repository.
- `local_agent::run` passes `prepared.cwd` as the broker workspace.

Focused tests (all PASS with `cargo test --bin temote-mcp --locked agent_git`):

- `broker_rejects_requests_for_a_different_workspace_and_preserves_its_metadata` — session permits
  A and B, run selects A, request for B rejected and B's HEAD/branch/index/worktree unchanged.
- `broker_rejects_symlinked_cwd_and_paths_outside_the_workspace` — symlink cwd and symlinked `add`
  path rejected; sibling sentinel unchanged.
- `broker_rejects_nested_repositories_and_linked_worktrees` — nested repo identity mismatch and
  linked worktree outside the scope rejected; linked worktree HEAD unchanged.
- `broker_rejects_a_workspace_that_is_not_a_git_worktree_root` — subdirectory selection rejected.
- Existing `broker_adds_and_commits_only_the_named_paths`,
  `broker_creates_switches_and_preserves_a_dirty_worktree`,
  `shim_and_broker_round_trip_over_the_private_directory` still PASS.

Known scope decision: selecting a subdirectory of a repository rejects broker mutations;
selecting the repository/worktree root is the supported shape.

Linked-worktree correction (2026-09-17 review): `BrokerScope::for_workspace` **accepts** a linked
worktree root (its `.git` pointer makes `git_worktree_root(root) == root`), and the broker serves
its supported mutations in the yolo path. In ask/agent mode the git command still fails closed in
`sandbox::run_git` because the primary repository's common metadata is outside `[worktree_root]`;
that limitation is owned by `issues/polished/20260917-linked-worktree-broker-metadata-scope.md`.
The earlier statement that a linked worktree run cwd is rejected was inaccurate.
