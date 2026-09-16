# host-side Git tag push / GitHub Actions dispatch を Temote から安全に実行できるようにする

Status: done / repository implementation complete
Model: gpt-5.6-sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 developer workflow friction

## 概要

Temote の normal/agent session では sandbox 内から GitHub への DNS/network access が遮断され、`gh` の OS credential/keyring も利用できない。一方、`git_push` は host-side credential を使って branch push を安全に実行できる。

release workflow が tag push または `workflow_dispatch` を要求する repository では、main の commit/push までは Temote で完了できても release trigger だけ実行できない。

## 2026-09-16 gh-git で観測した事実

Repository: `/home/hirohito-fujita/src/gh-git`

1. `temote.git_push` により `main` の `f070c60` push は成功した。
2. sandbox 内 `gh auth status --hostname github.com` は保存済みアカウントの token を利用できず exit 1。
3. sandbox 内 `git ls-remote --tags origin refs/tags/latest` は `Could not resolve host: github.com` で exit 128。
4. repository の `.github/workflows/release.yml` は `push.tags: [latest]` と `workflow_dispatch` で release を開始する。
5. Temote には host-side `git_commit`, `git_push`, `git_fetch`, `git_pull` はあるが、tag/ref push や GitHub Actions workflow dispatch の専用 mutation tool がない。
6. 1Password service-account host runner は当該 session で `configured=false`。
7. active session 一覧に同 repository を扱える yolo session は存在しなかった。

## 問題

- release/tag 操作だけ手動へ戻るため、approve-free な開発→検証→commit→push→release の完結性が失われる。
- sandbox に network を開放する必要はなく、既存 `git_push` と同様の host-side bounded operation で解決可能。
- generic shell/force push を許可するより、ref と repository を制約した専用 API の方が安全。

## 提案

### A. `git_push_tag` / `git_update_tag`

- configured remote のみ対象。
- source commit SHA と exact tag name を引数に取る。
- create と update を区別する。
- update は expected remote old SHA 必須の lease semantics にする。
- arbitrary refspec / URL / shell args は受け付けない。
- annotated/lightweight の扱いを明示する。

### Implemented first slice (2026-09-16)

`git_push_tag` を追加した。これは local tag object を作らず、exact local commit SHA を configured remote の `refs/tags/<tag>` へ直接 push するため、semantics は **lightweight remote tag** に限定する。annotated/signed tag object の生成はこの surface では扱わない。

入力は `session_id`, optional `cwd`, configured remote name（default `origin`）, unqualified `tag`, exact `source_sha`、optional `expected_remote_sha` だけ。arbitrary refspec / URL / raw push option / unconditional force は受け付けない。

- `source_sha` は 40/64 hex の完全 object ID だけを許し、local `^{commit}` として同じ完全 SHA に解決することを host-side read-only Git inspection で確認する。
- tag は `refs/tags/` をTemote側で付与し、`git check-ref-format` を通す。
- `expected_remote_sha` 省略時は `--force-with-lease=refs/tags/<tag>:` を使い、remote tag が存在しない場合だけ createする。
- update時は `--force-with-lease=refs/tags/<tag>:<expected_remote_sha>` を使い、remote refがexact old SHAから変化していればfail-closed。
- read-only preflight (`remote get-url`, `check-ref-format`, exact `rev-parse`) は permitted repository rootでTemote組み立てargvだけを host-side実行し、sensitive environment namesを除去する。generic `execute` のnetwork/sandbox capabilityは変更しない。
- gateway routed tool contractにも同じschemaを追加した。

Verification:

- `git_push_tag_` focused suite: 3/3 PASS。
- local bare remote E2E: create-only PASS、duplicate create with different SHA FAIL + remote unchanged、stale expected SHA FAIL + remote unchanged、exact expected SHA update PASS。
- `noprop`: 1,024 generated tag/source/expected combinationsで commandが `refs/tags/*` と exact lease/refspec から逸脱しないことを確認。
- activity coverage: 3/3 PASS。
- gateway contract snapshot generation: 1/1 PASS。
- gateway tests: 2/2 PASS after tool-count assertionsを49へ更新。
- existing `git_push` branch regression: local bare remoteで initial upstream push + subsequent non-force update 1/1 PASS。

### B. GitHub Actions workflow dispatch

- current repository の configured GitHub remote から owner/repo を解決。
- workflow file/id と ref のみを受け取る。
- repository-local managed Git credential mapping を利用し、token を MCP client や child environment に返さない。
- dispatch 後は run id を返し、exact run status / conclusion を poll 可能にする。raw logs/artifacts は別surfaceとする。

### Implemented second slice (2026-09-16)

`github_workflow_dispatch` と `github_workflow_run_get` を追加した。

- repository は caller input では受け付けず、selected configured remote の URL を read-only host Git inspection で取得し、`https://github.com/OWNER/REPO(.git)` / `git@github.com:OWNER/REPO(.git)` / `ssh://git@github.com/OWNER/REPO(.git)` の3形だけを bounded parser で受理する。
- workflow は numeric workflow ID または slash を含まない `.yml` / `.yaml` filename だけ。
- ref は unqualified branch/tag name だけを受け付け、Temote 側で Git ref grammar を確認する。
- dispatch は GitHub REST workflow-dispatch endpoint へ `return_run_details=true` を送り、推測検索ではなく response の `workflow_run_id` を直接返す。
- `github_workflow_run_get` は exact run ID の `status`, `conclusion`, `event`, `head_sha`, GitHub `html_url` だけを bounded response として返す。raw logs/artifacts はこの surface では取得しない。
- child process へ継承される `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN` は sensitive env list へ追加して除去する。GitHub API credential は ambient active `gh` account を使わず、repository-local Git config が helper reset + `!gh git credential --managed`、かつ `credential.useHttpPath=true` の exact mappingを持つ場合だけ approval 後に hidden managed helper `gh git credential --managed get` を current repository cwd で直接呼び出して内部解決する。`git credential fill` は URL-scoped global helperへ戻り得るため使わない。解決した token は bounded な direct GitHub REST request にだけ利用し、global `gh auth` stateを変更しない。
- GitHub API endpoint/body は Temote が構築し、caller は arbitrary repository / endpoint / raw HTTP/gh argv を指定できない。

Verification:

- GitHub repository/workflow/ref/run-id validation + response parsing: PASS。
- `noprop`: 1,024 generated owner/repo/workflow/ref combinations で dispatch endpoint が configured repository から逸脱しないことを確認。
- sensitive child environment `noprop`: PASS（GH/GITHUB token names を含む）。
- repository-scoped credential mapping tests: exact repo-local helper reset + `!gh git credential --managed` / `useHttpPath=true` のみaccept。current repo では `git config --get-urlmatch credential.helper https://github.com/f4ah6o/temote-mcp.git` が global `!/usr/bin/gh auth git-credential` を返すことを実測し、`git credential fill` では ambient account fallback を防げないと確認したため、hidden managed helperをdirect invocationする実装へ変更した。
- direct managed helper argv (`gh git credential --managed get`) を固定テストし、extra repo-local helperを generated/PBT 1,024 cases でreject。
- credential resolution: exact repo path is supplied to the verified `gh git credential --managed get` helper after repository-local mapping validation; parser rejects wrong host / duplicate or incomplete secret fields and never echoes the secret sentinel。`noprop` 1,024 cases PASS。
- activity operation serialization: PASS。activity advertised-tool coverage 3/3 PASS。
- public tool metadata: PASS。
- Rust gateway contract snapshot + gateway runtime: PASS / 2/2。
- live GitHub workflow dispatch は未実施。release/deploy workflow を勝手に起動する副作用を避け、明示的な実 live 対象がある場合だけ実施する。

## 受け入れ条件

- [x] normal/agent用の host-side tag trigger surfaceを、sandbox networkを開放せず実装した。外部GitHub credentialを使うlive tag pushは rebuilt/runtime acceptance待ち。
- [x] tag update は expected old SHA が一致しない場合 fail-closed（local bare remote E2Eで確認）。
- [x] arbitrary force push / arbitrary refspec / arbitrary URL はschema/command builder上許可しない。
- [x] workflow dispatch は configured repository と exact workflow/ref に限定される（pure/PBT で確認、live dispatch は未実施）。
- [x] tag/GitHub release toolsはcredential値を入力・返却しない。継承 `GH_TOKEN` / `GITHUB_TOKEN` / enterprise variantsは既知sensitive environmentとして扱い、GitHub API tokenは repository credential helperからapproval後に内部取得して `Zeroizing<String>` で保持し、child environmentではなくdirect RESTのAuthorizationにだけ使う。
- [x] exact run ID の `github_workflow_run_get` で queued/running/completed + conclusion を poll できる surface を実装した。live run tracking acceptance は未実施。
- [x] branch push の既存 safety contract を回帰させない（focused local bare remote regression PASS）。

## 2026-09-16 polishing completion

All repository-local acceptance items are checked. The intentionally unrun live GitHub workflow/tag invocation is now a row in `issues/open/20260908-live-acceptance-matrix.md`; it no longer keeps this implementation issue open.
