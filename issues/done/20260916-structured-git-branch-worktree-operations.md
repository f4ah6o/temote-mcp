# normal session で branch / worktree を安全に作成・切替できる structured Git operation がない

Status: done / structured branch + switch + repository-owned worktree add implemented and repository-local gates green
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 developer workflow friction
Type: Git / sandbox / developer workflow

## Observed

`/home/hirohito-fujita/src/local-mcp` の active normal Temote session (`permission_mode=agent`, `yolo=false`) で、既存の dirty `main` を触らず `.git/.wt/` 配下へ実装 worktree を分離しようとして次を実行した。

```text
git worktree add .git/.wt/friction-hunt -b feat/20260916-friction-hunt origin/main
```

結果:

```text
Preparing worktree (new branch 'feat/20260916-friction-hunt')
fatal: cannot lock ref 'refs/heads/feat/20260916-friction-hunt': Unable to create '/home/hirohito-fujita/src/local-mcp/.git/refs/heads/feat/20260916-friction-hunt.lock': Read-only file system
```

`git fetch` は dedicated `git_fetch` tool で成功し、`main` と `origin/main` は同一 SHA `8aa9d32709975d7f39b8cccb943ddf0ac44c33dc` だった。したがって失敗は stale ref ではなく、ordinary sandbox command から Git metadata mutation を行えない境界で再現している。

## Current contract / root cause

現行設計では ordinary `execute` / `start_command` は Git metadata を read-only に保つ。`AGENTS.md` も「ordinary sandboxed commands must not gain write access to Git metadata; dedicated Git tools を使う」と明記している。

一方、公開されている dedicated Git mutation は現在以下に限定される。

```text
git_add
git_commit
git_fetch
git_pull
git_push
```

`git_add` / `git_commit` は `sandbox::run_git` を通し、検証済み repository の Git metadata root だけを writable にする。network 系は host-side bounded operation を使う。しかし branch create/switch と worktree add/remove を表す structured operation は存在しない。

そのため、通常 clone / linked worktree のどちらでも、開発 agent が「dirty work を保持したまま新しい branch/worktree に分離する」という標準的な Git workflow を行う経路がない。これは単に branch 名の切替だけの問題ではなく、`refs/heads`、`HEAD`、index/worktree metadata、`worktrees/*` 等を更新する branch/worktree mutation 全般の capability gap である。

## Goal

normal `ask` / `agent` session の sandbox と Git metadata protection を維持したまま、一般的な開発 workflow に必要な branch/worktree 操作を bounded structured operation として提供する。

## Proposed surface

最低限、以下を評価する。

- `git_branch_create`
  - validated branch name
  - base は current HEAD / configured local ref / fetched remote ref など既存 repository 内の ref のみに限定
  - arbitrary refspec / URL は受け付けない
- `git_switch`
  - existing local branch または明示的に作成した branch のみ
  - dirty worktree を暗黙 discard しない
  - `--force`, `-f`, destructive reset 相当は公開しない
- `git_worktree_add`
  - destination は session permitted root 内か、repository-owned reviewed root（例 `.git/.wt/<safe-name>`）に限定
  - branch create と existing branch attach を明示的に区別
  - arbitrary external path / `--force` は公開しない
- 必要なら `git_worktree_remove`
  - clean / registered worktree のみを既定にし、`--force` は別設計にする

単一の raw `git` escape hatch や Git metadata 全体の broad write permission は追加しない。

## Implemented first slice (2026-09-16)

`git_branch_create`, `git_switch`, `git_worktree_add` を structured Git operations として追加した。

- ordinary `execute` / `start_command` の Git metadata read-only contract は変更しない。
- mutationは `ApprovalClass::LocalStructured` を通し、その後だけ validated repository Git metadata roots を writable にする。branch create / switch は既存 `sandbox::run_git`、worktree add は専用 `sandbox::run_git_worktree_add` を使う。
- branch name は bounded precheck + `git check-ref-format --branch` で検証する。option-like / `refs/...` / control character は拒否。
- `base` は `HEAD` または bounded repository-local/fetched ref/object ID grammarに限定し、`rev-parse --verify --end-of-options <base>^{commit}` で exact commit SHAへ先に解決する。`~`, `^`, `@{}`, range式等のrevision expressionは入力として拒否する。
- `git_branch_create` は `git branch --no-track <branch> <resolved-sha>` のみ。current worktreeは切り替えない。
- `git_switch` は existing local branch (`refs/heads/<branch>`) の存在を先に確認し、`git switch --no-guess <branch>` のみ。`--force`, reset, stashは公開しない。
- `git_worktree_add` のdestination inputは公開しない。supported rootを **`<repository>/.wt/<safe-name>`** と定義し、`.git/.wt` は採用しない。既存 `.wt` がsymlink/非directoryならfail-closed。
- worktree `base` 指定時は branch が未存在であることを確認して `git worktree add -b <branch> <repo>/.wt/<name> <resolved-sha>`。`base` 省略時は existing local branch だけを `git worktree add <destination> refs/heads/<branch>` でattachする。
- `git worktree add` は common `.git/worktrees` への metadata create が必要なため、通常の `run_git` の read-only maskを広げず専用 policy を追加した。exact structured operation 中だけ common `worktrees` parent の作成を許可し、開始時点で存在する sibling private metadata directory は個別に read-only maskする。linked worktree から実行する場合は current private metadataだけを sibling maskから除外する。
- `worktrees` metadata root がsymlink/非directory、または配下に想定外の非directory entryがある場合は fail-closed にする。
- force/reset/stash/arbitrary URL/refspec/arbitrary destination pathはschema/command builderに存在しない。

Repository-local verification:

- `structured_git_*` direct Git fixture: 4/4 PASS。branch create → switch → linked worktree add、existing branch attach、dirty tracked conflict switch rejectionを確認。
- 元worktreeのuntracked dirty fileは branch create / switch / worktree add の全工程で保持され、conflicting dirty tracked fileもswitch失敗後に内容・current branchとも保持されることを確認。
- `generated_git_worktree_destinations_stay_under_repo_owned_root`: `noprop` 1,024 cases PASS。
- `worktree_add_scope_*`: 2/2 PASS。existing sibling metadataを保護し、current linked-worktree private metadataだけを除外するscope計算と、unexpected metadata entryのfail-closedを確認。
- Linux policy unit test `worktree_add_policy_opens_parent_but_masks_existing_sibling_metadata`: PASS。
- macOS `SandboxSpec` に同等の `git_worktree_add_spec_opens_parent_but_masks_existing_sibling_metadata` testを追加。Linux上ではcompile/run対象外。
- Linux/macOS の実sandbox runtime testを追加し、actual `git worktree add` と sibling metadata write denialを同じfixtureで検証するようにした。このnormal Temote sessionではnested sandbox runtimeを実行できないため **NOT RUN (host/CI gate)**。
- `git_push_tag_is_create_only_by_default_and_updates_with_exact_lease`: PASS。
- `git_push_branch_behavior_remains_non_force_and_host_side`: PASS。
- `activity_approval_maps_all_git_operations_and_safe_remote_classes`: PASS。
- public tool metadata: 1/1 PASS、activity coverage: 3/3 PASS、gateway tests: 2/2 PASS。
- `cargo clippy --all-targets -- -D warnings`: PASS。
- `cargo check --no-default-features --all-targets --locked`: PASS（既存dead-code warningあり）。
- `just sandboxed-check`: exit 0。lib 106/106、activity job 5/5、activity coverage 3/3、upgrade transaction 40/40、upgrade coordinator 6/6、gateway 2/2、`git diff --check` PASS。
- `just sandboxed-check` が明示する host-only 項目は **NOT RUN (host/CI gate)**: Linux nested sandbox runtime tests / full binary-local Unix-socket integration / ignored supervisor-process-boundary E2E。
- 既存 `activity_approval_local_git_tools_keep_results_and_complete_once` をこのnormal sessionから直接起動すると `temote-linux-sandbox: bubblewrap is required for Linux sandboxing` でnested sandboxを開始できないことを実測。これをPASSとは扱わずhost/CI gateとして残す。

## Security constraints

- ordinary `execute` の `.git` read-only contract は維持する。
- public HTTP / normal session から arbitrary executable, raw argv, URL/refspec, force checkout/reset を公開しない。
- branch name は `git check-ref-format --branch` 相当で検証し、option injection を防ぐ。
- destination path は canonicalize / symlink containment を行い session roots 外へ出さない。
- linked worktree の common Git directory と private worktree metadata の対応を既存 `git_metadata_roots` と同等以上に検証する。
- dirty/untracked work を reset, checkout, stash, delete しない。
- hooks/signing/network は操作ごとの既存安全契約に従う。

## Acceptance criteria

- [x] clean repository で新しい local branchを作成するstructured surfaceを実装し、direct Git fixtureで確認。
- [x] existing local branchへforceなしでswitchするstructured surfaceを実装し、direct Git fixtureで確認。
- [x] repository-owned destinationに linked worktreeを作成するstructured surfaceを実装し、direct Git fixtureで確認。
- [x] supported destinationを `<repository>/.wt/<safe-name>` と正式化し、arbitrary destination inputを公開しない。
- [x] dirty current worktree の変更は branch create/switch/worktree add のfixtureで保持されることを確認。
- [x] option-like branch/base、unsafe revision expression、unsafe worktree name、path escape、force optionはvalidation/builder/PBTで拒否・非生成。
- [x] linked worktree の sibling metadata / unrelated Git repository metadata へ write capability が広がらない。専用policyはvalidated common Git root直下のexisting sibling private metadataだけをread-only maskし、unexpected entryはfail-closed。
- [x] existing `git_add` / `git_commit` / fetch/pull/push の safety contract と tests を回帰させない。通常 `run_git` policyは変更せず、release/network系の既存回帰testとdeterministic gateを維持。
- [x] Linux/macOS の structured Git sandbox tests と PBT を追加する。実runtime testはhost/CI gateとして追加済みで、このnormal sessionではNOT RUN。

## Test ideas

- branch name の generated PBT: valid subset は canonical form、`-x`, `..`, `@{`, control chars, slash edge cases 等は reject。
- destination path PBT: permitted root 内の相対 path は containment、`..`, absolute sibling, symlink escape は reject。
- clean repo: create branch -> switch -> commit。
- dirty repo: switch が overwrite を要求する場合 fail closed、元の変更が保持されることを確認。
- linked worktree: add -> private metadata only writable -> sibling worktree metadata denial。
- regression: ordinary `execute(["git","branch",...])` / `execute(["git","worktree","add",...])` は引き続き Git metadata write を得ない。

## Additional evidence: dirty-main scope contamination (2026-09-16)

この capability gap のため、`git_push_tag` 実装は既存 dirty `main` 上で継続せざるを得なかった。作業中に別プロセスが commit `2472a0c` (`feat: add lease-protected Git tag push`) を作成し push したが、その commit には tag-push scope に加えて、既存未コミットだった `codex_status` description 1行も取り込まれていた。

当該 description 自体を不正変更とは判定していないため revert はしない。しかし「他作業者の既存変更を保持しながら、今回分だけを isolated worktree で実装・stage・commit する」経路がないことにより、commit scope contamination が実際に発生した証拠である。

Acceptance では、structured worktree 作成後に現在 checkout の dirty/staged state が新規 worktree の commit に混入しないことも回帰確認する。
