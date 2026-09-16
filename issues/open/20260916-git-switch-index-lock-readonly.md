# Temote sandbox 内の `git switch main` が `.git/index.lock: Read-only file system` で失敗する

Status: open
Model: gpt-5.6-sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 developer workflow friction
Related:
- `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
- `issues/open/20260916-local-agent-process-spawn-eperm.md`
- `issues/done/20260916-structured-git-branch-worktree-operations.md`

## Observed behavior

Temote の sandbox 内で通常の Git UX として次を実行すると失敗する。

```text
git switch main
```

観測した error:

```text
.git/index.lock: Read-only file system
```

Git は branch switch の過程で index / repository metadata の更新を必要とするが、Temote の normal sandbox / local-agent sandbox は `.git` を protected metadata として read-only にしているため、通常の `git switch` をそのまま実行できない。

## Problem

agent mode で開発 agent に通常の Git command を使わせたい一方、`.git` 全体を writable にすると protected metadata boundary を崩す。

したがって本件は単純な filesystem permission 不足として `.git` を writable にするのではなく、通常の Git syntax を bounded Git broker へ転送する経路が必要である。

現状は public MCP surface に `git_switch` が存在するため orchestration 側から branch switch 自体は可能だが、Codex / OpenCode の通常 developer workflow では Temote 固有 MCP tool を知らないと `git switch` が失敗する。

## Expected behavior

`permission_mode=agent` の開発フローでは agent が通常の Git syntax を使える。

```text
git switch main
git switch <existing-branch>
git switch -c <new-branch>
```

その際、Temote は Git metadata root を broad writable にせず、validated operation として必要最小限の mutation を broker 側で実行する。

ordinary `execute` の protected `.git` read-only contract 自体は維持してよい。local-agent developer UX では private Git shim / internal Git broker により通常の Git syntax を吸収する。

## Acceptance criteria

- [ ] `permission_mode=agent` の `local_agent_run` で `git switch main` 相当が `.git/index.lock: Read-only file system` にならない。
- [ ] `git switch <existing-branch>` が validated branch switch として動作する。
- [ ] `git switch -c <new-branch>` が bounded branch creation + switch として動作する。
- [ ] `.git` root 全体を local agent に writable で公開しない。
- [ ] dirty / untracked work を reset / checkout-force / stash / delete しない。
- [ ] branch switch が競合する local modifications を検出した場合は fail-closed し、既存変更を保持する。
- [ ] `.git/index.lock` の read-only failure を generic Git failure に潰さず、protected metadata mutation path として診断可能にする。
- [ ] existing `git_switch` structured operation の validation / safety invariant を broker backend から再利用する。
- [ ] Linux / macOS で representative regression coverage を追加する。

## Scope note

根本対応は `20260916-agent-mode-git-broker-gh-git-integration.md` を正本とする。本 Issue は `git switch main` -> `.git/index.lock: Read-only file system` という具体的な developer-facing failure を固定し、Git broker 実装後の acceptance probe として追跡する。
