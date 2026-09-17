# Local-agent Git shim slice 2: add and commit

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: `20260916-git-shim-switch-create.md`

## Goal

Allow ordinary `git add <paths>` and `git commit -m <message>` inside local-agent development flow through bounded parent-side Git operations.

## Scope

- reuse existing `git_add`/`git_commit` safety semantics;
- support explicit relative paths inside current repository and one message form;
- protect unrelated dirty/untracked work from accidental staging;
- reject `-A`, `--all`, path escape, config/alias injection, hooks/signing override, and arbitrary commit plumbing in this slice.

Do not implement fetch/pull/push or worktrees here.

## Acceptance

- agent can edit one file, `git add` only that file, and commit it;
- unrelated modified/untracked files remain unstaged/unchanged;
- hooks/signing remain disabled as in structured Git tools;
- protected metadata is not broadly writable;
- focused/PBT path validation and `just sandboxed-check` pass.

## Implementation notes (2026-09-16)

Extended the packet 2.1 shim/broker (`src/agent_git.rs`) instead of adding new plumbing:

- `classify_argv` now accepts `add <path>...` (1..=`MAX_GIT_ADD_PATHS` explicit relative paths) and `commit -m|--message <message>` in addition to the switch forms. Everything else, including `-A`/`--all`, `.`, `..`, absolute paths, option-like paths, glob/pathspec syntax, `--`, `-p`, `-a`, `--amend`, `--no-verify`, `-S`, `-F`/`--file`, `--author`, `--date`, duplicate messages, pathspecs, empty messages, and over-long messages, is rejected before any Git process runs. The fixed rejection message now lists all supported forms.
- `add` paths are revalidated on the parent side against the prepared session roots: the path is resolved relative to the shim cwd, canonicalized (symlinks resolved; a missing target checks its canonical parent), and must stay inside the session roots. Git receives the explicit absolute paths with `core.hooksPath=/dev/null` and `--`, so unrelated dirty/untracked work is never staged implicitly.
- `commit` reuses the structured command builder (`build_git_commit_command`: `core.hooksPath=/dev/null`, `commit.gpgSign=false`, `--no-verify`, `--no-gpg-sign`, `-m`) and the shared message validation, and now also runs `ensure_staged_paths_are_permitted` before committing so the index cannot carry paths outside the session roots. That helper was made `pub(crate)` and runs host-side for yolo sessions (matching `run_git_and_report`) and in the restricted sandbox otherwise.
- `src/mcp.rs` only extracted shared helpers (`validate_git_path_syntax`, `validate_git_commit_message`, `build_git_commit_command`, `MAX_GIT_ADD_PATHS`, `MAX_GIT_COMMIT_MESSAGE_BYTES`, `ensure_staged_paths_are_permitted`) so the structured tools and the broker share one validation path; structured tool behavior is unchanged.
- `.git` remains read-only for direct agent writes: no writable roots were added.

Tests:

- `cargo test --bin temote-mcp --locked agent_git`: PASS (10/10) — classifier accept/reject matrices for add/commit (including glob, absolute, `..`, option injection, amend/no-verify/signing forms), real temporary-repository `add tracked.txt` + `commit -m` asserting the commit contains only the named file while an unrelated modified file stays uncommitted and an untracked file stays untracked, symlink-escape rejection, plus the existing switch/queue tests.
- `cargo test --bin temote-mcp --locked git_`: PASS (17/17) — structured `git_add`/`git_commit` and shared path validation did not regress.
- `just sandboxed-check`: exit 0 (lib 113/113, agent_git 10/10, gateway 71/71, clippy/no-default/diff clean).
- host/CI-only: NOT RUN (a real `local_agent_run` where an installed Codex/OpenCode invokes `git add`/`git commit` through the shim, macOS host execution, actual CI). Covered by the live matrix `Agent-mode Git shim` row.

## 2026-09-16 completion

All repository-local acceptance items are met. The parent umbrella stays open for the remaining Git shim packets.
