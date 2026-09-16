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
