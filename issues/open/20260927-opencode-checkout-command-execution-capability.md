# OpenCode implementation tasks need checkout + command execution

Status: open
Model: unknown
Created: 2026-09-27
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Bind OpenCode implementation tasks to a safe writable workspace and scoped command capability.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

A task can be accepted without a usable checkout or safe command capability, then fail after spending a model turn.

## 目標

Bind OpenCode implementation tasks to a safe writable workspace and scoped command capability.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: Non-goals

- OpenCode のモデル性能評価
- OpenCode 自体への新しい coding capability 実装
- task ごとに独自 clone / workspace 実装を増やすこと
- 既存 workspace / VCS contract を迂回する別系統の checkout 管理

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Preserved fixed contract: Requirements

### 1. Checkout provisioning

- task が対象とする repository / ref / workspace identity を解決する
- OpenCode task 開始前に実 working checkout が存在することを保証する
- checkout path を backend に明示的に渡す
- checkout がない場合、必要なら Temote が既存 repository-store / workspace contract を使って用意する
- 既存の未コミット変更を勝手に reset / checkout / stash / clean しない

### 2. Command execution capability

OpenCode implementation task には、少なくとも対象 checkout 内で以下を実行できる capability を提供する。

- repository inspection
- build
- test
- lint / check
- diff inspection
- `git status` または対応する VCS status

単に source tree を read/write できるだけでは implementation task の完了条件を満たさない。

### 3. Explicit task contract

OpenCode task metadata / runtime context には少なくとも次を含める。

- repository identity
- requested ref / base
- resolved checkout path
- effective cwd
- available execution capability

backend adapter がこれらを暗黙推測する設計にしない。

### 4. Preflight / fail-fast

implementation task 開始時に preflight を行い、最低限以下を区別する。

- repository を解決できない
- checkout を provision / locate できない
- checkout はあるが read/write できない
- command execution capability がない
- command execution はあるが cwd が checkout と一致しない

不足がある場合は、モデル実行を続けて「実装不能」という結果にするのではなく、capability / workspace blocker として明示する。

### 5. Error classification

checkout / command-execution 不足を、OpenCode のモデル能力不足として分類・表示しない。

エラーには少なくとも以下を含める。

- backend: OpenCode
- target repository
- target ref if known
- resolved / attempted checkout path if known
- missing capability
- recovery hint

## 受け入れ条件

Complete source criteria from “Acceptance criteria” (unchecked items remain unverified):

1. `f4ah6o/workflows.mbt` を対象に OpenCode implementation task を開始すると、OpenCode から対象 checkout を読み書きできる。
2. 同じ task context から repository 内で build / test / lint 等の command を実行できる。
3. 実装後に diff と VCS status を同じ checkout で確認できる。
4. checkout がないケースは、backend/model 実行前または開始直後に capability blocker として検出できる。
5. command execution capability がないケースを checkout 不足とは別に識別できる。
6. regression test が「checkout あり + execution あり」の成功ケースと、少なくとも各不足ケースを覆う。
7. 既存の未コミット変更を破壊しない。
8. ユーザー向け / agent 向け status で「OpenCode に実装能力がない」と誤分類しない。

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd gateway && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.

## 2026-10-05 実行パケット

- [`opencode-scoped-command-workspace`](../polished/20261005-opencode-scoped-command-workspace.md)
- [`managed-workspace-allocation`](../polished/20261005-managed-workspace-allocation.md)

These are planned packets, not completed implementation. The parent remains open until applicable children and acceptance evidence are complete.

## 既存設計・履歴

> Historical Status: open — confirmed task-provisioning gap
Repository: `f4ah6o/temote-mcp`  
> Historical Created: 2026-09-27 (Asia/Tokyo)
Related: `issues/closed/20260927-instruction-side-bare-repo-provisioning.md`, `issues/done/20260925-v2-vcs-workspace-contract.md`

Triage: this is not a duplicate of bare-repository provisioning. The bare-repo issue owns caller-side repository-store preparation; this issue owns execution-time binding of a writable checkout, effective cwd, and command capability to an OpenCode implementation task.

Polished child packet: `issues/polished/20260927-opencode-task-preflight-capability-blockers.md` (OC1, ready)。preflight / blocker 分類のみを先に実装し、実際の checkout provisioning は Phase F/C 側に残す。

Current-main evidence (2026-10-02): `src/opencode_server.rs::serve_permission_config` explicitly denies the OpenCode V2 `shell` capability for `*`. A fix must provide workspace-scoped execution with protected-state, cwd, approval, and containment boundaries; simply deleting the deny rule is not sufficient. This preserves the source evidence from PR #74.

## Summary

OpenCode を実装に使うには、Temote 側で対象 repository の checkout と command-execution capability を事前に用意する必要がある。

今回 `f4ah6o/workflows.mbt` を対象にした OpenCode task で確認した blocker は、OpenCode が実装能力を持たないことではない。OpenCode task に対象 checkout と command execution capability が提供されていなかったことが直接の blocker だった。

この差を Temote の execution contract として明示し、implementation task が workspace/capability 不足のまま開始されないようにする。

## Problem

OpenCode backend が利用可能でも、task に次のいずれかが欠けると実装を遂行できない。

- 実装対象 repository の working checkout
- checkout の実 path / cwd
- source を読み書きする capability
- build / test / lint / git status 等を実行する command-execution capability

この状態で task を開始すると、agent 側からは「実装できない」ように見える。しかし原因はモデル能力ではなく Temote が提供した execution environment の不足である。

特に remote / delegated backend では、backend が repository 名を知っていることと、その repository の実 checkout にアクセスできることを同一視してはいけない。

## Goal

実装を伴う OpenCode task を開始する際、Temote が対象 repository の working checkout と command-execution capability を確実に用意し、それらを task runtime に結び付ける。

## Requirements

### 1. Checkout provisioning

- task が対象とする repository / ref / workspace identity を解決する
- OpenCode task 開始前に実 working checkout が存在することを保証する
- checkout path を backend に明示的に渡す
- checkout がない場合、必要なら Temote が既存 repository-store / workspace contract を使って用意する
- 既存の未コミット変更を勝手に reset / checkout / stash / clean しない

### 2. Command execution capability

OpenCode implementation task には、少なくとも対象 checkout 内で以下を実行できる capability を提供する。

- repository inspection
- build
- test
- lint / check
- diff inspection
- `git status` または対応する VCS status

単に source tree を read/write できるだけでは implementation task の完了条件を満たさない。

### 3. Explicit task contract

OpenCode task metadata / runtime context には少なくとも次を含める。

- repository identity
- requested ref / base
- resolved checkout path
- effective cwd
- available execution capability

backend adapter がこれらを暗黙推測する設計にしない。

### 4. Preflight / fail-fast

implementation task 開始時に preflight を行い、最低限以下を区別する。

- repository を解決できない
- checkout を provision / locate できない
- checkout はあるが read/write できない
- command execution capability がない
- command execution はあるが cwd が checkout と一致しない

不足がある場合は、モデル実行を続けて「実装不能」という結果にするのではなく、capability / workspace blocker として明示する。

### 5. Error classification

checkout / command-execution 不足を、OpenCode のモデル能力不足として分類・表示しない。

エラーには少なくとも以下を含める。

- backend: OpenCode
- target repository
- target ref if known
- resolved / attempted checkout path if known
- missing capability
- recovery hint

## Acceptance criteria

1. `f4ah6o/workflows.mbt` を対象に OpenCode implementation task を開始すると、OpenCode から対象 checkout を読み書きできる。
2. 同じ task context から repository 内で build / test / lint 等の command を実行できる。
3. 実装後に diff と VCS status を同じ checkout で確認できる。
4. checkout がないケースは、backend/model 実行前または開始直後に capability blocker として検出できる。
5. command execution capability がないケースを checkout 不足とは別に識別できる。
6. regression test が「checkout あり + execution あり」の成功ケースと、少なくとも各不足ケースを覆う。
7. 既存の未コミット変更を破壊しない。
8. ユーザー向け / agent 向け status で「OpenCode に実装能力がない」と誤分類しない。

## Example failure

今回の観測:

> OpenCode を実装に使うには、まず Temote 側の workflows.mbt checkout を用意する必要があります。今回確認した blocker は「OpenCode が実装能力を持たない」ことではなく、この OpenCode task に checkout と command-execution capability が提供されていないこと

これは backend capability の問題ではなく、Temote -> OpenCode task handoff の workspace / execution provisioning 問題として扱う。

## Related design

既存の repository store / workspace / VCS transaction 設計を再実装せず、それらを OpenCode backend の task provisioning に接続する。

関連:

- `issues/open/20260924-temote-development-harness-restructure.md`
- `issues/done/20260925-f1-repository-store-workspace-contract.md`
- `issues/done/20260925-v2-vcs-workspace-contract.md`
- `issues/open/20260925-vcs-transaction-jj-first.md`

## Non-goals

- OpenCode のモデル性能評価
- OpenCode 自体への新しい coding capability 実装
- task ごとに独自 clone / workspace 実装を増やすこと
- 既存 workspace / VCS contract を迂回する別系統の checkout 管理
