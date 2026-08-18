# リポジトリ履歴から旧公開ドメイン（example.com）と実名メールアドレスを除去する

Status: done
Model: claude-sonnet-5
Created: 2026-08-06
Updated: 2026-08-06
Branch: chore/20260806-purge-example-domain-history

## 概要

`temote-mcp` の working tree と全 git 履歴から、旧公開ドメイン `localmcp.example.com` および実名を含むテストメールアドレスを除去し、新ドメイン `example.com` に置き換える。

## 背景

`temote-mcp` の公開 HTTP エンドポイントは Cloudflare Access Managed OAuth で保護されており、README には運用ドメインとして旧ドメイン `localmcp.example.com` が記載されていた（[README.md](../../README.md)）。`origin`（`f4ah6o/temote-mcp`）は当時 private リポジトリだった。なお、当時この issue では `upstream`（`nakasyou/local-mcp`）を「無関係の別プロジェクト」と認識していたが、公開準備時の再確認で、本リポジトリが同 upstream から派生したことを明示する方針へ訂正した。運用ドメインを `temotemcp.example.com` に切り替えるにあたり、旧ドメインと、[src/http.rs](../../src/http.rs) のテストコードに含まれる実名メールアドレス（プレースホルダー: `test@example.com` に置換）を履歴からも除去する。

このイシューは [20260806-reconnect-oauth-example-domain.md](../open/20260806-reconnect-oauth-example-domain.md) の前提作業であり、Cloudflare 側の再設定より先に完了させる。

## 問題

- `README.md` と `.env.example` の working tree に旧ドメイン文字列が計8箇所、2ファイルにわたって残っていた。
- 全履歴中に旧ドメイン文字列の出現が12件、実名メールアドレスの出現が1件あった（`git log --all -p` で確認済み）。
- `feat/oauth-http-server` ブランチは `main` への未マージ差分が `.DS_Store` の ignore 追加1コミットのみで、主要変更は PR #1 で `main` に既にマージ済みであり、残す理由がない。

## 目標

- working tree と全コミット履歴の両方から旧ドメイン文字列と実名メールアドレスが見つからない状態にする。
- `README.md` と `.env.example` が新ドメイン `example.com` を参照している。
- `feat/oauth-http-server` ブランチがローカルと `origin` の両方から削除されている。
- `origin/main` の履歴が書き換え後の内容に更新されている。

## 対象外

- Cloudflare Access / Tunnel 側の再設定（[20260806-reconnect-oauth-example-domain.md](../open/20260806-reconnect-oauth-example-domain.md) で扱う）。
- `~/.config/temote-mcp/public.env` の値の設定（同上）。
- `opz`（1Password CLI ラッパー）のトラブルシューティング。生の `op item list --format json`（vault 指定なし）が `authorization timeout` になる一方、`--vault Personal` 指定時や単発の直接呼び出しは数秒で成功する。`opz` はad-hoc署名の自己ビルドバイナリで、再ビルドのたびに1Password側の承認がリセットされている可能性がある。原因調査はスコープ外とする。

## 提案する方針

1. 現在のリポジトリ全体を別ディレクトリへ `git clone` してバックアップを作る（force push 前の復元用）。
2. `feat/oauth-http-server` ブランチをローカルおよび `origin` から削除する。
3. `git-filter-repo`（`/opt/homebrew/bin/git-filter-repo` にインストール済み）の `--replace-text` を使い、全 blob に対して旧ドメイン文字列と実名メールアドレスを新ドメイン `example.com` およびプレースホルダー `test@example.com` に置換する。
4. 書き換え後の working tree で `README.md` と `.env.example` の内容を確認し、新ドメイン表記に矛盾がないか確認する。
5. `origin/main` へ force push する。

## 受け入れ条件

- [ ] `git log --all -p` に旧ドメイン文字列が含まれない（0件）。
- [ ] `git log --all -p` に実名メールアドレスが含まれない（0件）。
- [ ] working tree に旧ドメイン文字列・実名メールアドレスが含まれない（`target/` と `.git/` を除く）。
- [ ] `git branch -a` に `feat/oauth-http-server` が存在しない。
- [ ] `origin/main` が書き換え後の履歴で更新されている。
- [ ] filter-repo 実行前のバックアップ clone が存在する。

## テスト計画

- `cargo test` （filter-repo後もソースの内容変化がテスト結果に影響しないか確認）
- `git log --all -p` で旧ドメイン文字列・実名メールアドレスの残存有無を確認
- working tree 全体（`target/`、`.git/` 除く）で残存有無を確認
- `git branch -a` でブランチ削除を確認
- バックアップ clone から差分を diff し、意図した置換のみが行われたことを確認

## リスク

- `git filter-repo` は全コミットの SHA を変更する不可逆操作であり、force push 後は旧履歴への参照（ローカルの他クローン、CI キャッシュ等）が無効になる。
- force push によって `origin/main` の履歴が書き換わるため、他にこのリポジトリをクローンしている環境がある場合は再クローンが必要になる。
- `--replace-text` の置換対象文字列が短いと意図しない箇所まで置換される可能性がある。実行前に置換ルールと対象ファイルを目視確認する。

## 変更履歴

`CHANGES.md` impact: no

## 注記

- 2026-08-18 公開準備時に lineage を再確認し、本リポジトリは `nakasyou/local-mcp` から派生したものとして README と third-party notice で明示する方針に訂正した。GitHub の fork metadata は持たない。
- 本イシューの実行はコマンド実行のみで完結し、Cloudflare ダッシュボード側の操作は不要。
- 2026-08-06: バックアップ clone 作成、`feat/oauth-http-server` 削除（ローカル・origin）、`git filter-repo --replace-text` 実行、`origin/main` へ force push、GitHubからのクリーンcloneで受け入れ条件を全て確認済み。`cargo test` の4件の失敗はfilter-repo前のバックアップでも再現する既存の環境依存問題（macOSの `/tmp` symlink、Seatbelt）であり、本作業とは無関係と確認した。
