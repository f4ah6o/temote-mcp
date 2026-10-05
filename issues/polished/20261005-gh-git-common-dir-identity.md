# C0: repository-scoped gh-git identity from common dir

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

C0: repository-scoped gh-git identity from common dir. Parent: `issues/open/20260924-temote-development-harness-restructure.md`.

## 背景

The umbrella lists linked-worktree profile resolution by git common dir as unfinished; gh-git is an external owned integration.

## 問題

The parent phase is not fully implemented or verified.

## 目標

Complete this bounded phase with the parent safety contract intact.

## 対象外

Unrelated backends, unsupported integrations and a broader security model change are outside this packet.

## 提案する方針

In the gh-git repository, resolve repository-scoped profile from the canonical git common dir for linked worktrees, preserving explicitly selected account and current security boundary. In Temote, consume the resulting stable identity without changing global gh auth active account. Coordinate as an external dependency; do not edit gh-git from this issue-normalization branch.

## 受け入れ条件

- [ ] Main and linked worktree select the same repo-scoped identity.
- [ ] Unrelated repositories remain isolated.
- [ ] Explicit account mapping wins.
- [ ] No global gh auth mutation.

## テスト計画

gh-git fixture tests for linked worktrees and account mapping; Temote integration canary after dependency lands; host credential gate NOT RUN until available.

## リスク

Never print or copy credential values into issues, logs or test output.

## 変更履歴

Assess compatibility and user-facing impact when implemented; add a `CHANGES.md` entry if applicable. This issue preparation does not edit it.

## 注記

- 2026-10-05: Split from `issues/open/20260924-temote-development-harness-restructure.md`; no implementation or test PASS is claimed.
