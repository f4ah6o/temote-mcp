# host-side Git tag push / GitHub Actions dispatch を Temote から安全に実行できるようにする

Status: doing / lease-protected tag trigger implemented; GitHub workflow dispatch remains
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
- host-side GitHub credential を利用し、token を MCP client に返さない。
- dispatch 後は run id を返し、poll/status/log summary まで追跡可能にする。

## 受け入れ条件

- [x] normal/agent用の host-side tag trigger surfaceを、sandbox networkを開放せず実装した。外部GitHub credentialを使うlive tag pushは rebuilt/runtime acceptance待ち。
- [x] tag update は expected old SHA が一致しない場合 fail-closed（local bare remote E2Eで確認）。
- [x] arbitrary force push / arbitrary refspec / arbitrary URL はschema/command builder上許可しない。
- [ ] workflow dispatch は configured repository と exact workflow/ref に限定される。
- [x] tag preflight/pushはcredential値を入力・返却せず、host childから既知sensitive environment namesを除去する。
- [ ] release workflow の run state を running → completed/failed まで追跡できる。
- [x] branch push の既存 safety contract を回帰させない（focused local bare remote regression PASS）。
