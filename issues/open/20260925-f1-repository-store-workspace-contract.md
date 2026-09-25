# F1: repository store / workspace JSON・ref / path / freshness 契約

Status: contract delivered (設計 packet。実装コードなし)
Packet: F1 (Phase F — initial repository store / no-local-main foundation の第 1 child)
Repository: `f4ah6o/temote-mcp` (この文書) / `f4ah6o/gh-git` (Git 側 API の実装対象)
Branch / observed HEAD: temote-mcp `main` `c69bfe3` (A1 merge 後), gh-git `main` `890d2f5`
Parent issue: `issues/open/20260924-temote-development-harness-restructure.md` (PR #47)
Prerequisites: 現状確認のみ (実装 packet の成果に依存しない)

## VCS substrate review

この F1 は repository identity / bare store / remote-tracking freshness / no-local-main の generic contract として維持する。

ただし `git worktree` / `store-worktree` を新規 managed workspace の最終 substrate とする部分は、
`issues/open/20260925-vcs-transaction-jj-first.md` の V1/V2 decision 対象になった。

**V1 は完了し jj-first viable と判定済み。F2/F3 を git-worktree-only として実装しない。V2 contract (`issues/open/20260925-v2-vcs-workspace-contract.md`) に従って backend-neutral に実装する。**

V1 は jj-first acceptance を満たした。以下は V2 で backend-neutral contract に改訂済み:

- `git worktree add` 固定
- `layout=store-worktree` 固定
- branch-only workspace identity
- workspace mutation / snapshot semantics

host / owner / name、bare Git store、origin freshness、legacy checkout 保全、Temote reservation ownership は可能な限り保持する。
既存 checkout を jj へ自動変換・reset・clean・stash しない。

`issues/open/20260925-v2-vcs-workspace-contract.md` を workspace substrate の authoritative contract とし、本 F1 は repository identity / bare store / freshness / legacy protection の generic contract として参照する。

## 1. Goal

`RepositoryStore + Workspace` と remote-tracking ref / freshness の generic contract を固定する。workspace substrate 固有部分は V1 の jj feasibility 結果を受けて V2 で確定し、その後 F2–F4 / C1–C2 が利用する。
この packet は契約書だけを成果とし、実装・wire format の実動作検証は行わない。

## 2. Fixed decisions

### 2.1 Repository identifier

repository の識別子は **`host` / `owner` / `name` の 3 要素**とする。同名 repo を directory
basename だけで同一視しない。

```json
{ "host": "github.com", "owner": "f4ah6o", "name": "temote-mcp" }
```

- 入力は `owner/repo` (host は configured default)、`host/owner/repo`、HTTPS / SSH URL
  (`https://host/owner/repo.git`、`git@host:owner/repo.git`) を受理し、上の 3 要素に
  normalize する。`.git` suffix と大小は GitHub の規則で normalize するが、一意性の判定は
  normalize 後の 3 要素一致とする。
- bare の `repo` 名だけの指定は **store ensure では受理しない** (`owner_required`)。
  inspect / remove 等の shorthand は store 内で一意に解決できる場合だけ受理し、複数候補は
  `repo_ambiguous` で失敗する。remote owner 名から GitHub login を推測しない。
- 現行 `<src-root>/<repo>` layout は basename identity のまま `primary-checkout` layout と
  して分類し (2.7)、新 store への自動 mapping はしない。

### 2.2 Store layout と store record

store root は呼出し側が決める絶対 path (`--store-root` または `GH_GIT_STORE_ROOT`)。
Temote は自身の trusted `src` root 配下の専用 namespace (例: `<src-root>/repositories/`)
を渡し、gh-git は渡された root の canonical 性だけを検証する。root 未指定時の default は
`$XDG_DATA_HOME/gh-git/store` (fallback `~/.local/share/gh-git/store`)。

store root 配下の layout (v1):

```text
<store-root>/<host>/<owner>/<repo>.git          # bare store (作業 checkout を持たない)
<store-root>/<host>/<owner>/<repo>.workspaces/<workspace_id>/   # task worktree
<store-root>/<host>/<owner>/<repo>.git/gh-git/  # gh-git 管理 registry (非 Git 公開面)
```

host / owner / repo の各 path 要素は `[A-Za-z0-9][A-Za-z0-9._-]*` に制限する。
`..` / 絶対 path / separator を含む指定は `path_escape`。

bare store の Git 設定:

```text
remote.origin.url    = <clone url>
remote.origin.fetch  = +refs/heads/*:refs/remotes/origin/*
```

`refs/heads/main` の作成・維持を必要としない。`git worktree add` は base commit から
task branch を直接切るため、store に local main が存在しない状態を標準とする。

初回 fetch 成功後に remote の HEAD symref を解決し (`git remote set-head --auto` 相当)、
`remote.default_branch` へ記録する。解決不能なら `default_branch: null` を記録し
(`base_unresolved` 参照)、推測で埋めない。

gh-git は Git から導出できない field だけを registry (`<store>/gh-git/`) に保存する:
workspace の request fingerprint、作成時の base 記録、schema_version。registry は
secret を含まず、write は temp + rename の atomic update。registry だけを真実とせず、
`git worktree list` / refs / remote config と照合して返す。

`gh git store inspect <repo> --json`:

```json
{
  "schema_version": 1,
  "repository": { "host": "github.com", "owner": "f4ah6o", "name": "temote-mcp" },
  "store": {
    "path": "/u/src/repositories/github.com/f4ah6o/temote-mcp.git",
    "canonical_path": "/u/src/repositories/github.com/f4ah6o/temote-mcp.git",
    "layout": "bare",
    "store_version": 1
  },
  "remote": {
    "url": "https://github.com/f4ah6o/temote-mcp.git",
    "default_branch": "main",
    "fetch_refspecs": ["+refs/heads/*:refs/remotes/origin/*"]
  },
  "observations": {
    "refs/remotes/origin/main": {
      "commit": "c69bfe3b12117b70b8160091e54dce97cf49ec3a",
      "observed_at": "2026-09-25T01:30:00Z"
    }
  },
  "fetch": {
    "last_attempt_at": "2026-09-25T01:30:00Z",
    "last_success_at": "2026-09-25T01:30:00Z",
    "last_error": null
  },
  "freshness": "fresh",
  "workspaces": ["task-api"]
}
```

### 2.3 Workspace record

`gh git workspace inspect [--repo <repo>] [--id <workspace_id>] --json`
(repo / id 省略時は cwd を検査。store-worktree ならこの record、
legacy / unmanaged path なら 2.7 の record を返す):

```json
{
  "schema_version": 1,
  "workspace_id": "task-api",
  "repository": { "host": "github.com", "owner": "f4ah6o", "name": "temote-mcp" },
  "layout": "store-worktree",
  "path": "/u/src/repositories/github.com/f4ah6o/temote-mcp.workspaces/task-api",
  "canonical_path": "/u/src/repositories/github.com/f4ah6o/temote-mcp.workspaces/task-api",
  "branch": "feat/api",
  "head": { "commit": "1a2b3c...", "attached": true },
  "base": {
    "ref": "refs/remotes/origin/main",
    "source": "remote-tracking",
    "commit": "c69bfe3b12117b70b8160091e54dce97cf49ec3a",
    "observed_at": "2026-09-25T01:30:00Z"
  },
  "status": {
    "dirty": false,
    "untracked": false,
    "detached": false,
    "ahead": 0,
    "behind": 0,
    "diverged": false
  },
  "created_request_fingerprint": "sha256:ab12..."
}
```

- `workspace_id` は caller 指定の必須 id。`[A-Za-z0-9][A-Za-z0-9._-]{0,63}`、先頭 `.` 不可、
  `/` / `..` を含まない。branch 名からの暗黙 flatten はしない (現行 Temote の
  `derive_task_name` とは別の契約; C 系 packet が mapping を決める)。
- `branch` は必須。remote の `default_branch` と同名の branch 指定は `branch_reserved`
  (no-local-main 前提を壊さないため)。
- `base` は `refs/remotes/origin/*`、store 内の task branch 名 (stacked task の親)、
  または commit SHA を受理し、ensure 時に commit へ解決して記録する。
- `--base` 省略時は `refs/remotes/origin/<remote.default_branch>` に normalize する。
  `default_branch: null` では省略指定を `base_unresolved` とし、明示 `--base` は受理する。
- `base.source` は `remote-tracking` | `task-branch` | `commit` を記録する。
  `base.observed_at` の意味は source ごとに決める: `remote-tracking` はその ref の
  成功 fetch 観測時刻、`task-branch` / `commit` は ensure 時の解決時刻
  (remote の観測とは区別する)。

### 2.4 ensure の同一性

`gh git store ensure <host/owner/repo> [--json]`:

- store が無ければ bare init + origin + refspec 設定 + 初回 fetch。既存なら設定の
  照合 (url / refspec が一致すれば再利用、不一致は `store_conflict`)。
- fetch 成功時に `observations` と `fetch.last_success_at` を更新して返す。

`gh git workspace ensure <repo> --name <id> --branch <b> [--base <ref|sha>] --json`:

- 同一性は normalized request `{repository, workspace_id, branch, base}` の fingerprint
  (sha256) で判定し、作成時に registry へ記録する。`base` には normalize 後の ref 名
  (省略なら `refs/remotes/origin/<default_branch>`) または指定された commit SHA を使い、
  解決済み commit 自体は fingerprint に含めない。remote が進んだ後の同一再送は
  `reused` のままとする。
- 同一 fingerprint の再送は **再利用** (`"result": "reused"`)。新規作成は
  `"result": "created"`。応答の record 形状は 2.3 と同じ。
- 同一 `workspace_id` で異なる request (branch / repo / base が違う) は
  `workspace_conflict`。既存内容を上書きしない。
- ensure は **network I/O を行わない**。freshness は呼出し側が `store fetch` で制御する。
  base が remote-tracking ref の場合は記録済み observation に解決し、未観測なら
  `base_unverified`。明示 commit SHA はそのまま受理。
- 対象 path が既に存在し managed record を持たない場合は `path_occupied`。
  既存 user path の削除・採用はしない。

成功応答:

```json
{
  "schema_version": 1,
  "result": "created",
  "workspace_id": "task-api",
  "repository": { "host": "github.com", "owner": "f4ah6o", "name": "temote-mcp" },
  "path": "/u/src/repositories/github.com/f4ah6o/temote-mcp.workspaces/task-api",
  "branch": "feat/api",
  "base": {
    "ref": "refs/remotes/origin/main",
    "source": "remote-tracking",
    "commit": "c69bfe3b12117b70b8160091e54dce97cf49ec3a",
    "observed_at": "2026-09-25T01:30:00Z"
  }
}
```

### 2.5 Error contract

失敗は exit code 非 0 + `--json` 時に構造化 error を stdout へ返す:

```json
{
  "schema_version": 1,
  "error": {
    "code": "workspace_conflict",
    "message": "workspace task-api exists with a different request",
    "detail": { "existing_branch": "feat/other" }
  }
}
```

| code | 条件 |
| --- | --- |
| `repo_ambiguous` | shorthand repo 指定が複数 store に一致 |
| `owner_required` | store ensure で owner を解決できない |
| `store_missing` / `store_conflict` | store 不在 / ensure 時の url・refspec 不一致 |
| `workspace_missing` | inspect / remove 対象が無い |
| `workspace_conflict` | 同一 id・異なる request fingerprint |
| `path_occupied` | 対象 path に unmanaged の既存内容 |
| `path_escape` | id / repo 要素・root が namespace を逸脱、または非 canonical / symlink root |
| `branch_reserved` | remote default branch 同名の workspace branch |
| `branch_exists` | 指定 branch が store 内に既存 (別 base / 別 workspace 所有) |
| `base_unverified` | base が remote-tracking ref で成功済み fetch 記録が無い |
| `base_unresolved` | base ref / commit を解決できない |
| `fetch_failed` | fetch 失敗。`detail.category`: `network` / `auth` / `remote`。**secret を載せない** |
| `dirty_workspace` | remove / prune 対象が dirty・未提出 commit・ahead を含む (`--force` なし) |
| `legacy_layout` | store 専用操作に `primary-checkout` / `external` layout を渡した |
| `schema_version_unsupported` | 未知のより新しい schema_version の record |

### 2.6 Freshness contract

freshness は「最後に成功した fetch の観測」と「呼出し側の policy」の評価であり、
store 側は観測事実だけを保持する。失敗した fetch は観測を更新しない。

- `fetch.last_success_at` / `observations.<ref>.{commit, observed_at}` が真実の source。
- `store fetch` は prune 意味 (`git fetch --prune` 相当) を持つ。成功時は observations を
  現在の remote-tracking ref 集合で**置き換える**: 上流で削除された branch の ref は
  除去され、fresh な base として残らない。失敗は `fetch.last_attempt_at` と
  `fetch.last_error` のみ更新し、last_success / observations を触らない。
- `store inspect` の `freshness` field は caller の `--max-age-seconds` に対する評価:
  - `fresh`: last_success_at が max_age 以内
  - `stale`: 成功観測はあるが max_age 超過 (直近 attempt 失敗を含む。last_error が区別に使える)
  - `unverified`: 成功した fetch が一度も無い
- 「最新」とは絶対に報告しない。remote が進んだ可能性は常に残り、Temote は task 開始・
  再開・delivery 前に `store fetch` し、返った commit / observed_at を evidence に記録する。

### 2.7 Layout 判別と legacy 保全

legacy / unmanaged 対象は `gh git workspace inspect --path <dir>` で指定する (省略時の
cwd 検査と同じ規則)。`<dir>` が store-worktree なら 2.3 の record、`primary-checkout` /
`external` なら以下の legacy record、git worktree でなければ `workspace_missing`。
legacy record は read-only で `preserved: true` を付け、生成・変更・削除を一切しない:

```json
{
  "schema_version": 1,
  "layout": "primary-checkout",
  "workspace_id": null,
  "repository": { "host": "github.com", "owner": "f4ah6o", "name": "temote-mcp" },
  "path": "/u/src/temote-mcp",
  "canonical_path": "/u/src/temote-mcp",
  "branch": "main",
  "head": { "commit": "c69bfe3...", "attached": true },
  "status": {
    "dirty": false,
    "untracked": false,
    "detached": false,
    "ahead": 0,
    "behind": 0,
    "diverged": false
  },
  "preserved": true
}
```

legacy 側の `repository` は `remote.origin.url` を解析できた場合のみ埋め、不能なら
`null` (basename や owner 名から推測しない)。`ahead` / `behind` / `diverged` は
remote-tracking ref との比較で評価し、比較対象の観測が無い場合は `null`。

inspect は対象を layout で分類する:

- `store-worktree`: 新標準 (bare store + 管理 worktree)
- `primary-checkout`: 現行 canonical `<src-root>/<repo>` checkout
- `external`: 上記以外の通常 git worktree / checkout

`primary-checkout` / `external` の inspect は **read-only**。dirty / 未提出 commit /
ahead / diverged を `status` で報告するが、reset / delete / 移動 / 強制同期を行わない。
legacy repo の新標準への移行は明示 opt-in の別操作 (F4 packet 以降) であり、inspect が
「移行済み」を偽らない。新標準と legacy compatibility は別状態として報告する。

### 2.8 Invariants

1. `store inspect` / `store list` / `workspace inspect` / `workspace list` は read-only。
   fetch・ref 書換・registry 書換を行わない。
2. `workspace ensure` は network を触らない。freshness の更新は `store fetch` / `store ensure` のみ。
3. `refs/heads/main` は通常操作で不要。integration は remote-tracking ref と PR 経由。
4. task の commit は専用 branch に残る。workspace が default branch 名を使わない。
5. path はすべて canonical。symlink / swap / traversal は `path_escape` で fail closed。
6. `remove` / `prune` は dirty・untracked・未提出 commit・ahead を含む対象を `--force` なしで
   消さない。`prune` は対象ごとに removed / skipped (理由付き) を報告する。
7. record / error / registry に token・credential を含めない。
8. basename だけで repository を一意視しない (host / owner / name)。
9. schema_version を全 record / 応答に付ける。未知の新 version は fail closed。
10. Temote の既存 reservation / admission (`WorktreeReservation` / `RepositoryReservation`)
    と競合する変更を gh-git 側で行わない。mutating op の呼出し前に Temote が reservation を
    取得する責務は Temote に残る。

## 3. Temote / gh-git boundary (Git 側 API 確定)

gh-git が提供する Git primitive (本 contract で固定。Phase C はこの表を再設計しない):

| command | effect | 性質 |
| --- | --- | --- |
| `gh git store ensure <repo>` | bare store 作成 / 照合 + fetch | mutating + network |
| `gh git store fetch <repo>` | remote-tracking 更新 | mutating + network |
| `gh git store inspect [repo]` / `store list` | 2.2 の record | read-only |
| `gh git workspace ensure <repo> --name --branch [--base]` | worktree 作成 / 再利用 | mutating (network なし) |
| `gh git workspace inspect [id]` / `list` / `--path <dir>` | 2.3 / 2.7 の record | read-only |
| `gh git workspace remove <id> [--force]` | worktree 除去 | mutating |
| `gh git workspace prune` | 条件付き回収 + 報告 | mutating |
| `gh git bind` / `binding status` 他 | 既存 identity 系 | 変更なし (C0 で profile 解決を修正) |

- `workspace` は gh-git の reserved management namespace (passthrough collision policy
  `issues/open/20260916-passthrough-command-collision-policy.md` と整合。Git 実コマンドは
  `worktree` であり `workspace` ではないため衝突しない。`gh git -- workspace` は実 Git へ)。
- `gh git worktree` (実 Git passthrough) はそのまま。Temote は managed 操作に `workspace`
  namespace を使い、generic passthrough で store を触らない。

Temote 側が所有し gh-git に移さないもの:

- repository / workspace_id の選択、task との binding、reservation / writer 排他。
- freshness policy (max_age、fetch する timing、失敗時の開始可否)。
- remove / prune の適格判定 (delivered / merged の検証) と、他 task の worktree を
  触らない invariant、legacy worktree の adopt / move / delete 禁止。
- 上記 mutating op 呼出し前の reservation 取得、および結果の evidence 記録。

AGENTS.md の管理操作 / コード操作の境界 (umbrella §Responsibility boundaries 対応):
この contract の store / workspace / fetch / remove 系は **Temote 固定 adapter の管理操作**
(typed input のみ、caller から raw argv / env / path を受けない) であり、agent が行う
コード操作ではない。実行権限の範囲: agent mode では許可済み repo / workspace / 操作範囲内の
store ensure・fetch・workspace ensure・inspect を再承認なしに実行し、`ask` では従来の
承認経路を維持し、scope 外や `--force` 系破壊操作は新たに黙認しない。policy test と
AGENTS.md 更新は実装 packet (F2–F4 / C1) の範囲とし、この packet では contract のみ固定する。

## 4. Read / change scope (現状確認した実在物)

temote-mcp (`main` `c69bfe3`):

- `src/managed_worktree.rs`: `ManagedRepository` (`<src-root>/<repo>` + `<src-root>/worktrees/<repo>`、
  basename identity)、`WorktreeReservation` / `RepositoryReservation` / `WorktreeAdmission`、
  `SessionWorkspace` (`canonical_checkout` / `managed_worktree` / `legacy_worktree`)、
  `derive_task_name`、`MAX_MANAGED_TASK_BYTES=64`。
- `src/sandbox.rs`: `git_primary_checkout` / `git_worktree_root` / `git_current_branch` /
  `git_common_dir` / `run_git_worktree_add` (broker)。
- `src/config.rs` + `src/named_roots.rs`: `state_dir()` (0700 owner-only)、`TEMOTE_MCP_ROOTS`
  named-root authority。
- `src/orchestration.rs` (A1 成果): 共通入口は task lifecycle 用。store 操作は含まない。

gh-git (`main` `890d2f5`):

- `internal/app`: passthrough + `bind` / `binding` / `env` / `shell-init`。store / workspace
  系 command は未実装。
- profile は `.git/gh-git/gh-config` (common dir 基準の修正は C0)。

この packet の変更は本ファイルのみ。既存コード・既存 issue への変更なし。

## 5. Acceptance (F1 の完了条件)

- [x] repository 識別子が host / owner / name を区別する (2.1)
- [x] store path / workspace_id / canonical_path / branch / base commit / last fetch /
      freshness / schema_version の具体 JSON がある (2.2–2.3)
- [x] ensure 同一要求再送、既存 branch 衝突、path escape、fetch 失敗の具体 error がある (2.4–2.5)
- [x] 新標準と legacy の判別、inspect read-only、dirty / 未提出 commit の保全条件がある (2.6–2.8)
- [x] Temote (所有権) / gh-git (Git primitive) の境界と Git 側 API が固定されている (3)
- [x] 管理操作の実行権限の意図が明記されている (3)

## 6. Validation

- docs-only packet。`git diff --check` と JSON 例の parse 確認を実施。
- runtime acceptance (実コマンド動作) は F2–F4 の範囲であり、この packet に PASS を付けない。

## 7. Out of scope (次 packet へ残す項目)

- gh-git 側の `store` / `workspace` 実装 (F2: bare store + inspect、F3: workspace ensure、
  F4: freshness / legacy 保全・回収条件)。
- Temote 側の `ManagedRepository::primary_checkout` → `RepositoryStore + Workspace`
  抽象化と compatibility adapter (owner/repo mapping 含む)。
- 移行 opt-in、recovery / GC / orphan、macOS / Linux acceptance。
- 管理操作の policy test 更新と AGENTS.md 反映 (実装 packet で実施)。

## 8. Completion report

packet F1 — 契約書を `issues/open/20260925-f1-repository-store-workspace-contract.md`
として納品。実装・runtime 検証は未実施 (設計 packet のため)。残件は §7 の通り F2–F4 /
C1–C2 / Temote 側抽象化 packet。
