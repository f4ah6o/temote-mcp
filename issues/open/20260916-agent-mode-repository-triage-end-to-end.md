# agent mode session だけで repository branch / PR / worktree triage を完遂できるようにする

Status: open
Model: GPT-5.6 Sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 developer workflow friction
Type: agent mode / Git / GitHub / worktree / repository triage

Related:
- `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
- `issues/done/20260916-structured-git-branch-worktree-operations.md`
- `issues/doing/20260916-repo-scoped-github-account-selection.md`
- `issues/closed/20260916-local-agent-process-spawn-eperm.md`
- `issues/closed/20260916-readonly-git-inspection-safety-block.md`
- `issues/closed/20260916-gh-auth-status-misleading-in-sandbox.md`

## Summary

`permission_mode=agent` の Temote session から、実 repository の branch / worktree / PR triage を最後まで行いたい。

今回 `obara-group-dev/kintone-monorepo` の branch triage を実施し、次までは agent mode の Temote orchestration で進められた。

- `git_fetch` による remote 更新
- local / remote branch の列挙
- merge-base / patch-id / unique commit の比較
- dirty worktree を保持したままの調査
- 別 repository の owner contract 確認を、別 Temote session を起動して実施
- rescue 対象 / migrated / superseded / cleanup 候補の分類
- local Codex へ rescue 実装を委譲し、test / typecheck / check / build / diff review を実施

しかし、cleanup と GitHub PR 操作まで含む end-to-end triage は、現在の agent mode surface だけでは完遂できなかった。

この Issue は個別 blocker の置き換えではなく、既存 Issue を束ねて **「通常の repository triage が agent mode session で最後まで終わる」** ことを acceptance とする umbrella tracker である。

## Target workflow

代表的な triage は次を一連で実行できる必要がある。

```text
session / repo / dirty state 確認
  -> fetch --prune
  -> local / remote branch / worktree 列挙
  -> PR state / branch association 確認
  -> current main との差分・unique patch 分類
  -> 必要成果を repo-owned isolated worktree へ rescue
  -> tests / diff review
  -> obsolete PR close
  -> merged / superseded remote branch delete
  -> local branch / worktree cleanup
  -> prune orphan metadata
  -> final git status / worktree list / remote branch list
```

この全工程を `--yolo`、global `gh auth switch`、unrestricted host shell、Git metadata broad write permission なしで実行する。

## Observed blockers

### B1. repository 実装済み capability と稼働 runtime/tool surface が一致していない

`/home/hirohito-fujita/src/local-mcp` の current repository HEAD は:

```text
69371fc feat: add structured package manager broker
```

であり、次の実装 commit は HEAD の ancestor であることを確認した。

```text
72e8f97 feat: add structured Git branch and worktree operations
```

一方、active session `temo` から確認した running binary は:

```text
temote-mcp 2026.9.9
```

現在 ChatGPT 側へ公開されている Git tool surface を discovery すると、`git_add`, `git_commit`, `git_fetch`, `git_pull`, `git_push` は見えるが、repository 側で実装済みの `git_branch_create`, `git_switch`, `git_worktree_add` は現在の runtime surface に現れない。

つまり repository code が capability を持っていても、実際に orchestration 中の session がその capability を利用できるとは限らない。

#### Needed

- running runtime version / build SHA / exposed tool capability を一貫して確認できること。
- upgrade/restart 後に「repository では実装済みだが現在 process には未反映」を明示的に検出できること。
- client が存在しない tool を前提に作業計画を立てないよう、capability discovery を authoritative にすること。

### B2. worktree remove / prune が structured operation として完結しない

今回、既に `main` へ取り込まれた worktree を cleanup するため ordinary sandbox command で `git worktree remove` / `git worktree prune` を試した。

実測:

```text
error: failed to delete '.git/worktrees/reconcile-main': Read-only file system
```

その結果、worktree directory 自体はなくなったが Git metadata が残り、`git worktree list` は次を表示した。

```text
prunable gitdir file points to non-existent location
```

これは cleanup operation が directory deletion と common Git metadata cleanup の間で部分完了した状態である。

`issues/done/20260916-structured-git-branch-worktree-operations.md` の first slice は branch create / switch / worktree add までで、remove / prune は end-to-end triage にまだ不足している。

#### Needed

- `git_worktree_remove`
  - repository-owned worktree のみ
  - clean worktree が既定
  - dirty / untracked があれば fail-closed
  - target worktree の private metadata だけを削除
  - sibling worktree metadata を変更しない
  - `--force` は初期 surface に含めない
- `git_worktree_prune`
  - stale metadata を列挙してから bounded cleanup
  - current / live sibling を prune しない
- directory と metadata の部分完了を避ける transaction / recovery contract

### B3. local branch delete / remote branch delete の safe structured path がない

triage 後は、merged / migrated / superseded と確認できた branch を cleanup する必要がある。

現在の structured Git surface では branch create/switch の実装はあるが、cleanup 用の local branch delete が end-to-end workflow に揃っていない。また `git_push` は current branch の non-force push に限定され、arbitrary refspec を受け付けないため remote branch delete を表現できない。

この制限自体は安全上正しいが、別の bounded delete operation がないため triage が終了しない。

#### Needed

- `git_branch_delete`
  - exact local branch name
  - current branch は拒否
  - attached worktree がある branch は拒否
  - dirty data を削除しない
  - 原則 fully merged / expected target ancestor を確認
  - force delete は初期 surface に含めない
- `git_remote_branch_delete`
  - configured safe remote のみ
  - exact branch name
  - default/protected branch を拒否
  - expected remote SHA / lease を必須または強く優先
  - arbitrary URL / arbitrary refspec を公開しない
  - concurrent update 時は fail-closed

### B4. PR の read / close を agent mode session 内で実行できる GitHub broker がない

今回 open PR #4 / #5 / #6 を内容・時点・現在設計との整合で triageし、superseded / obsolete と判定した。

しかし normal sandbox から `gh` を使うと GitHub network operation は成立しない。

実測したエラー:

```text
error connecting to api.github.com
```

normal `execute` の network disabled contract は維持すべきであり、`gh` に unrestricted outbound network を与えるのは解決策ではない。

一方、現在 Temote public tool surface には repository-scoped GitHub PR broker がなく、PR state確認・close・commentを Temote sessionだけで完了できない。

#### Needed

Git command broker と分離した bounded GitHub operation。最低限:

- current configured repository の open PR list
- exact PR get
- branch -> PR association lookup
- PR close
- triage reason comment の追加（必要な場合）

PR close は repository slug を caller の arbitrary URL から受けず、configured remote から解決する。

認証は `issues/doing/20260916-repo-scoped-github-account-selection.md` の repo-local credential contract を利用し、ambient `gh` active account に依存しない。

### B5. local agent の通常 Git UX と host-side credential/network broker がまだ一体化していない

今回の rescue では local Codex が fresh worktree を作成し、`apps/information-device-procedure` の成果を安全に分離できた。一方、Codex 側の `git fetch origin main` は:

```text
Repository not found
```

となり、Temote structured `git_fetch` では同じ repository の fetch が成功している。

また rescue worktree が canonical repository の `git worktree list` / branch list から見えず、別 clone / 別 common-dir 上で作られた可能性を追加確認する必要が生じた。

これは direct Codex workflow と Temote-owned canonical repository / credential routing が一致していないと、rescue自体が成功しても後続 cleanup / commit / push の authority が分裂することを示す。

`issues/open/20260916-agent-mode-git-broker-gh-git-integration.md` の private Git shim + internal broker がこの主解決策である。

#### Needed

- local agent は通常の `git ...` syntax を使用する。
- branch/worktree creation は canonical repository identity から Temote が導出する `~/src/worktrees/<repo>/<task>` に固定し、agent に配置先を選ばせない。
- existing `.wt/*` や sibling legacy worktree は discovery 対象にはするが、新規taskの標準workspaceとして自動採用・移動・削除しない。
- fetch/pull/push は Temote host-side broker + repo-scoped credential routing を使う。
- local agent 自身へ unrestricted network / Git metadata broad write を与えない。
- rescue completion 時に parent orchestration から同じ branch/worktree が必ず観測できる。

### B6. multi-repo triage evidence は現在 parent orchestration に依存する

今回の branch 判定では、同一 repository の diff だけでは不十分だった。

例:

- `docs/20260914-app1274-detail-sort` が `billone` へ移管済みか
- `docs/20260825-integration-platform` が `obara-integration-platform` で superseded か
- `docs/20260904-commercial-case-registry` の owner-side contract が `role-policy` へ反映済みか

これらは別 repository を read-only で確認して初めて、delete / preserve の判断ができた。

現在は ChatGPT parent orchestration が別 Temote session を起動して確認できるが、単一 `local_agent_run` は current session root の外を読めない。この path containment は維持すべきである。

#### Needed / design choice

次のどちらかを明示的 contract にする。

1. **multi-repo triage は parent orchestration responsibility** と定義し、local agent へは必要 evidence を入力する。
2. local agent が parent broker に対して、pre-approved named root の **read-only evidence request** を発行できる仕組みを設ける。

どちらの場合も、local agent の permitted root を sibling repository 全体へ broad に広げない。

### B7. triage completion を一つの high-level result として検証する surface がない

branch / worktree / PR cleanup は複数 operation を跨ぐ。途中で1つだけ失敗すると「どこまで安全に終わったか」を人間が再構築する必要がある。

今回も worktree directory 削除後に metadata cleanup が失敗し、`prunable` state を別途確認する必要があった。

#### Needed

少なくとも final verification recipe / optional structured helper として次をまとめて確認できること。

```text
current branch + dirty state
registered worktrees + prunable state
local branches + upstream
remote branches
open PRs
running jobs
```

mutation transaction 全体を1 toolにまとめる必要はないが、各 operation は idempotent / retry-safe で、最終状態を parent が再構成できる必要がある。

## Proposed end-to-end architecture

```text
ChatGPT / compatible MCP client
        |
        v
Temote agent session
        |
        +--> local_agent_run
        |      |
        |      +--> normal git syntax
        |             |
        |             v
        |        private Git shim
        |             |
        |             v
        |        internal Git broker
        |
        +--> structured Git cleanup operations
        |      branch delete
        |      worktree remove/prune
        |      remote branch delete
        |
        +--> repository-scoped GitHub broker
        |      PR list/get/close/comment
        |
        +--> optional bounded cross-repo read evidence
        |
        `--> final repository state verification
```

## Security constraints

- ordinary `execute` / `start_command` の network-disabled contract を維持する。
- ordinary sandbox command に Git metadata broad write を与えない。
- `--yolo` を triage の前提にしない。
- global `gh auth switch/login/logout` を自動実行しない。
- remote owner 名から GitHub login を推測しない。
- raw token / credential helper secret response を MCP output / logs / activity / issueへ出さない。
- force push / hard reset / force checkout / force worktree remove / arbitrary refspec / arbitrary remote URL を初期 surface に含めない。
- dirty / untracked work、他作業者 worktree、sibling private Git metadata を削除・stash・resetしない。
- cross-repo support を実装する場合も named-root / read-only / explicit scope を維持する。

## Acceptance criteria

### Runtime / capability consistency

- [ ] running Temote process の version / build SHA / exposed capability を client が確認できる。
- [ ] repository code に実装済みでも running runtime に未反映の tool を「利用可能」と誤認しない。
- [ ] structured branch/worktree capability を含む version へ upgrade/restart 後、tool discovery に実際に現れる。

### Git triage

- [ ] agent mode session から fetch --prune -> branch/worktree inspection -> rescue -> cleanup を実行できる。
- [ ] local agent が作成した rescue branch/worktree を parent Temote session から同じ canonical repository の object として観測できる。
- [ ] `git_worktree_remove` は clean repo-owned worktree を directory + metadata とも安全に削除する。
- [ ] worktree remove failure が directory-only deletion / metadata orphan を silently残さない。
- [ ] stale worktree metadata を bounded prune できる。
- [ ] fully merged / reviewed local branch を safe delete できる。
- [ ] exact expected SHA に紐づく remote branch を safe delete でき、concurrent update は拒否する。
- [ ] default/protected/current/attached/dirty branch・worktreeを誤削除しない。

### GitHub PR triage

- [ ] configured repository の PR list/get を normal agent session から bounded host-side operation で取得できる。
- [ ] exact PR を close できる。
- [ ] PR close 前後で global `gh` active account が変化しない。
- [ ] organization-owned repositoryでも repo-local approved credentialを使用し、owner名からloginを推測しない。
- [ ] ordinary sandbox / local agent に unrestricted GitHub network を与えない。

### Cross-repo evidence

- [ ] multi-repo判定を parent orchestration responsibility とするか、bounded read-only evidence brokerを提供するかを明文化する。
- [ ] どちらの方式でも sibling repo の write capabilityを current local agentへ暗黙付与しない。

### Regression scenario

- [ ] dirty main + unrelated untracked files +複数linked worktreeを持つ fixture を用意する。
- [ ] remoteに merged branch / obsolete PR / active rescue branch を用意する。
- [ ] agent modeだけで active成果を rescueし、tests後、obsolete PR/branch/worktreeだけをcleanupする E2E を追加する。
- [ ] 元のdirty/untracked内容と他作業者worktreeが byte-for-byte / ref-level で保持される。
- [ ] 最終 `git status`, `git worktree list`, remote branches, open PR state が期待値と一致する。
- [ ] ordinary sandbox / secret isolation / Git metadata protection の既存 tests が green。

## Non-goals

- unrestricted host shellをagentへ与えること
- yoloを通常developer workflowへ戻すこと
- arbitrary Git subcommand / GitHub API endpointをraw passthroughすること
- branch cleanupのために他作業者のdirty workをstash/reset/deleteすること
- one sessionのpath rootを無制限に sibling repositoryへ拡大すること

## 2026-09-16 polished execution queue

This file is now an umbrella/tracking issue. After the Git shim packets, execute:

1. `issues/polished/20260916-github-pr-broker.md`
2. `issues/polished/20260916-agent-repository-triage-e2e.md`

Cross-repo writes remain parent-orchestrator scope; do not broaden a local-agent session root.
