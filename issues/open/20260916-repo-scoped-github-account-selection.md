# host-side GitHub operation が ambient `gh` active account に依存しない repo-scoped credential selection を必要とする

Status: doing / direct managed-helper selection implemented; live host credential acceptance pending
Model: GPT-5.6 Sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 developer workflow friction
Type: GitHub CLI / credential routing / structured host operations

## Observed

`git_push_tag` 後続として `github_workflow_dispatch` / `github_workflow_run_get` の実装が進行している。実装は configured Git remote URL から exact `owner/repo` を解決して `gh api repos/<owner>/<repo>/...` を host-side 実行する。

しかし installed GitHub CLI 2.95.0 の `gh api --help` には account 選択用 `--user` がない。認証は `GH_TOKEN` / `GITHUB_TOKEN` が優先され、それらがなければ gh の保存済み active account に依存する。一方 `gh auth token --help` は `--user` を持ち、未指定時は active account の token を選ぶと明記している。

同じ host の `gh auth status --hostname github.com` では現在:

```text
fujita-obr  Active account: true
f4ah6o      Active account: false
```

repository `/home/hirohito-fujita/src/local-mcp` の configured remote は `https://github.com/f4ah6o/temote-mcp.git` である。

さらにこの repository は既に `.git/config` から `.git/gh-git/git-config` を include しており、credential helper は次の repo-local managed helper に置き換えられている。

```text
[credential]
    helper =
    helper = !gh git credential --managed
    useHttpPath = true
```

`git config --show-origin --get-all credential.helper` でも `.git/gh-git/git-config` の `!gh git credential --managed` が有効であることを確認した。したがって Git fetch/push/tag は repository-specific credential routing を利用できる一方、plain `gh api` はこの Git credential helper 経路を利用しない。この非対称性は今回の `gh-git` 導入目的そのものと衝突する。

したがって plain `gh api` は repository slug を `f4ah6o/temote-mcp` に固定しても、認証主体まで repository-specific に固定しない。active account がその repository への Actions write/read 権限を持たなければ失敗し、operator に `gh auth switch --user ...` を要求する摩擦が再発する。

## Important distinction

remote owner と GitHub login は一般には一致しない。organization repository（例 `obara-group-dev/...`）では owner は organization で、実 credential は個人/サービスアカウント/GitHub App である。したがって `owner == account` と推測して自動選択してはいけない。

また global `gh auth switch` を structured operation 内で実行すると、別 repository / terminal / agent の account state を変更するため不可。

## Goal

configured repository と approved credential identity の対応を Temote が明示的・non-global に解決し、host-side GitHub API operation が ambient `gh` active account に依存しないようにする。

## 2026-09-16 implementation update

workflow-dispatch working tree では plain `gh api` / ambient active account を使う方式を廃止した。selected configured remote から repository を固定したうえで、repo-local Git config が次の exact contract を持つ場合だけ処理する。

```text
credential.helper = <reset>
credential.helper = !gh git credential --managed
credential.useHttpPath = true
```

当初はこの mapping を検証後 `git credential fill` を使う案だったが、current repo で次を実測した。

```text
git config --get-urlmatch credential.helper https://github.com/f4ah6o/temote-mcp.git
!/usr/bin/gh auth git-credential
```

つまり global URL-scoped helper が generic repo-local helper より lookup 時に選ばれ得るため、事前検証だけでは ambient account fallback を防げなかった。

修正版は `git credential fill` を使わず、repo-local contract に記録されている hidden helperと同一の `gh git credential --managed get` を current repository cwd で direct invocationする。gh-git 実装を確認し、この helper が current repo の binding stateから `binding.Host + binding.Account` を明示して `gh auth token --hostname ... --user ...` を呼び、global active accountを変更・選択しないことを確認した。helper childから `GH_TOKEN` / `GITHUB_TOKEN` 系を除去し、stdout captureと返却 tokenをzeroizeする。GitHub REST requestはTemoteのbounded `reqwest` clientから直接行う。

deterministic tests は exact helper argv、extra local helper rejection、host/protocol/credential parser、secret non-echo、repository/path constructionを固定している。live host credential storeを使った GitHub workflow dispatch は rebuilt runtime acceptance待ちであり、未確認をPASSにはしない。

## Candidate directions

- host config に repository/remote -> GitHub auth identity の non-secret mapping を持たせる。
- per-invocation isolated `GH_CONFIG_DIR` 等で account selection state を閉じ込め、global gh config を mutation しない方式を評価する。
- `gh auth token --user ...` を利用する場合は raw token を MCP output/log/audit/argv に出さず、process environment exposure と child-process inspection boundary を明示的に設計する。
- Git credential helper と GitHub API credential routingを統合できるか評価する。ただし Git remote credential が必ず Actions API permission を持つとは仮定しない。
- 特に `gh git credential --managed` が管理する repo-specific credential を、raw token を child argv/stdout に露出させず Temote 内部 capability として再利用できるか評価する。Git credential protocol の secret-bearing response を通常 tool output へ流してはいけない。
- GitHub App / service credential を使う場合も repository scope と permission を明示し、ambient user account fallbackをしない。

## Security constraints

- `gh auth switch/login/logout` の global mutation を自動実行しない。
- repository owner から account login を推測しない。
- raw token / Authorization header / keyring contents を parent-visible output、activity、friction store、logs に残さない。
- arbitrary repository slug/URL を caller に渡させず、configured remote から解決する既存方針は維持する。
- credential selection failure を `token invalid` と雑に分類せず、mapping missing / permission denied / unavailable を区別する。

## Acceptance criteria

- [ ] 同一 host に複数 GitHub account が登録され、global active account が対象 repo 用 account でなくても structured GitHub operation が正しい approved credential で実行できる。
- [ ] organization-owned repository でも owner 名から login を誤推測しない。
- [ ] operation 前後で global `gh auth status` の active account が変化しない。
- [ ] concurrent repositories が別 credential identity を使っても相互に account state を奪い合わない。
- [ ] repository/credential mismatch は bounded fixed error として fail-closed。
- [ ] token値は client/output/log/auditに露出しない。
- [ ] test fixture は複数account + 複数repo mapping と concurrent execution を再現する。
