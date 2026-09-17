# Git shim: allow bounded read-only Git commands in the agent sandbox

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: none

## Current code and contract

The private `bin/git` shim accepts only `switch <branch>`, `switch -c|--create <branch>`,
`add <path>...`, and `commit -m <message>` (`src/agent_git.rs:63`); every other argv, including
`status`, `diff`, `log`, `show`, `rev-parse`, and `ls-files`, exits 128. The shim was specified as
the mutation broker, so this is a contract gap rather than a missing branch: ordinary review work
inside `local_agent_run` cannot even run `git status` through `PATH`.

## The one responsibility to change

Add a bounded read-only command path that never promotes privilege:

- the shim classifies a fixed allowlist of read-only subcommand + argument shapes;
- allowed commands execute the trusted Git executable inside the same agent sandbox (child of the
  shim), never through the parent broker and never through an unclassified argv pass-through;
- the trusted Git path is resolved by `local_agent` from the pre-shim PATH and passed privately;
  `TEMOTE_MCP_GIT_EXECUTABLE` is not agent-authored;
- environment is sanitized: `GIT_OPTIONAL_LOCKS=0`, `GIT_CONFIG_NOSYSTEM=1`,
  `GIT_CONFIG_GLOBAL=/dev/null`, pager disabled, and `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`/
  `GIT_CONFIG_*`/`GIT_EXEC_PATH`/`GIT_EXTERNAL_DIFF` removed;
- `.git` stays read-only in the sandbox; no mutation form is added and no writable root changes.

Allowlist (documented in the issue and code):
`status`, `diff`, `log`, `show`, `rev-parse`, `ls-files` with a fixed per-command flag set, bounded
numeric option values, safe revision tokens, and bounded relative paths after `--`. Everything else,
including `-c`, `--config*`, `--git-dir`, `--work-tree`, `--exec-path`, `--output`, `--ext-diff`,
aliases, hooks, and option-like values, is rejected with the fixed message.

## Not changing

- Mutation forms and the broker path are unchanged.
- No `.git` write access, no sandbox widening, no host-Git fallback for unclassified argv.
- Public tool schemas and structured Git tools are unchanged.

## Focused tests

- classifier accept/reject matrices for the six subcommands, including injection shapes.
- `run_shim` with the broker directory set and a fake trusted Git executable: read-only argv runs
  the fake executable, creates no broker request; mutation argv still goes to the broker.
- real temporary repository: `git status --porcelain`, `git diff`, `git log --oneline -n 1`,
  `git show HEAD`, `git rev-parse HEAD`, `git ls-files` produce real output.
- PATH-first shim scenario: resolving the trusted Git skips the shim itself.

## Host / CI / provider verification

- Linux host/CI focused tests; macOS host execution and a real Codex/OpenCode run are live-matrix
  rows.

## Completion condition

Read-only review commands work inside agent mode without any privilege or scope increase; focused
tests and `just sandboxed-check` pass.

## Implementation notes (2026-09-17)

Changes:

- `src/agent_git.rs`: `validate_read_only_git_argv` is a strict allowlist for `status`, `diff`,
  `log`, `show`, `rev-parse`, `ls-files` with fixed flag sets, bounded decimal counts, safe
  revision tokens (no `:` rev:path syntax), and bounded relative paths after `--`. `-c`,
  `--config*`, `--git-dir`, `--work-tree`, `--exec-path`, `--output`, `--ext-diff`, aliases,
  hooks, mutation forms, and every unlisted shape are rejected with the existing fixed message
  (which now lists the read-only forms).
- `shim_route` sends mutations to the broker, read-only forms to the trusted Git executable, and
  everything else to rejection; read-only execution never touches the broker.
- `run_read_only_git` executes the parent-selected Git path with `--no-pager`, disabled hooks and
  fsmonitor, `GIT_OPTIONAL_LOCKS=0`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`, `cat`
  pagers, and removes agent-provided `GIT_*`/pager variables including `GIT_DIR`, `GIT_WORK_TREE`,
  `GIT_INDEX_FILE`, `GIT_CONFIG_*`, `GIT_EXEC_PATH`, and `GIT_EXTERNAL_DIFF`.
- `src/local_agent.rs::AgentState::apply_to_environment` resolves the trusted Git from the
  pre-shim `PATH` (skipping the shim itself) and exports `TEMOTE_MCP_GIT_EXECUTABLE`; `run` keeps
  that canonical executable visible as a read-only file when it lives under a hidden root. No
  writable root, `.git` permission, or public schema changed.

Focused tests (PASS):

- `read_only_classifier_accepts_only_the_documented_forms` /
  `read_only_classifier_rejects_mutation_and_injection_shapes` — allow/deny matrices.
- `shim_routes_mutations_to_the_broker_and_read_only_commands_to_git` — pure routing matrix.
- `run_shim_executes_read_only_git_without_touching_the_broker` — real repository `git status`
  through the shim path, no broker request created; missing Git path and unknown subcommands are
  rejected.
- `resolve_trusted_git_skips_the_private_shim` — a PATH whose first entry is a `git` shim resolves
  to the real Git in the next entry.
- `read_only_git_executes_real_repository_commands` — `status`/`diff`/`log`/`show`/`rev-parse`/
  `ls-files` exit 0 against a real temporary repository.

NOT RUN: macOS host execution; a real Codex/OpenCode invoking `git status` through the installed
shim on a rebuilt runtime (live matrix row).
