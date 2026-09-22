# sandbox 内 `gh auth status` が network/credential isolation を「token invalid」と誤認させる

Status: folded into repo-scoped Git network packet
Model: GPT-5.6 Sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 developer workflow friction
Type: GitHub CLI / diagnostics / sandbox

## Observed

active normal `agent` session `temo` から次を実行した。

```text
gh auth status --hostname github.com
```

結果は exit 1 で、保存済み2アカウントに対して次の形式を返した。

```text
X Failed to log in to github.com account <account>
- The token in default is invalid.
- To re-authenticate, run: gh auth login -h github.com
- To forget about this account, run: gh auth logout ...
```

一方、同じ開発環境では Temote の host-side Git operation (`git_fetch` / `git_push`) が configured remote に対して動作している。また normal `execute` は設計上 network-disabled であり、host credential/keyring をそのまま child へ渡す契約でもない。

したがって、この `gh auth status` の表示だけから host 上の GitHub credential が壊れているとは判断できない。sandbox から API/token validation が成立しない状態と、本当に token が失効・破損している状態が operator-facing output では区別できていない。

## Problem

agent が表示を文字どおり解釈すると、正常な host credential に対して `gh auth login` / `gh auth logout` / account switch を提案・実行する方向へ誘導される。これは不要な credential mutation と account-global state 変更を発生させ得る。

Temote の設計では Git network mutation を structured host-side tool に寄せているため、sandbox 内 raw `gh auth status` を authoritative readiness check として扱うべきではない。

## Goal

GitHub capability の診断で、少なくとも次を明確に区別する。

- host-side credential + configured repository operation が利用可能
- sandbox child からは network / OS credential integration のため検証不能
- host-side credential 自体が実際に失敗
- account mismatch / repository permission不足

## Proposed direction

- bounded read-only `github_status` / `git_remote_status` のような host-side diagnostic を評価する。
- repository の configured remote のみ対象とし、host credential を client へ返さない。
- raw token、keyring contents、`gh auth token` は公開しない。
- network-disabled ordinary `execute` で `gh auth status` が失敗した場合は、その結果だけで credential invalid と結論しないことを Agent Skill / operator docs に記載する。
- `git_push_tag` / GitHub workflow dispatch 等の structured operation は、その実 operation の host-side preflight/result を authoritative evidence とする。

## Acceptance criteria

- [ ] normal sandbox 内の raw `gh auth status` failure を host credential invalid と自動分類しない。
- [ ] supported host-side diagnostic が configured repository への GitHub reachability/auth readiness を secret-free に返せる。
- [ ] credential unavailable / network unavailable / permission denied を固定 classification で区別する。
- [ ] token値、Authorization header、keyring contents、account secretを出力しない。
- [ ] diagnostic のために `gh auth switch/login/logout` 等の mutation を自動実行しない。
- [ ] agent/operator guidance が sandbox-child status と host-side structured operation の結果を区別する。

## 2026-09-16 polishing disposition

No separate capability is required. Sandbox-child `gh auth status` guidance and host-side readiness classification are folded into `issues/done/20260916-git-shim-network-gh-git.md` and the existing repo-scoped credential tracker.
