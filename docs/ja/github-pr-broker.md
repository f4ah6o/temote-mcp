# GitHub pull request ブローカー

ブローカーは、管理対象セッション、`cwd`、Git リモートで選ばれたリポジトリに対し、範囲を限定した一覧取得、詳細取得、クローズを行います。

一覧は、未クローズの pull request を `updated_at` の降順で最大 50 件返します。ラッパーメタデータは `repository`、`limit`、`possibly_truncated` です。`limit` は常に 50、`possibly_truncated` は返却件数が 50 件のときだけ true です。これは保守的な表示であり、51 件目が存在することを保証しません。

各 pull request の概要フィールドは **number, title, state, draft, head_ref, updated_at, html_url** のみです。

詳細取得とクローズでは、`number` に正規形の正の十進文字列を指定します。

```json
{ "session_id":"temo", "number":"7" }
```

この例は GET/CLOSE 用であり、一覧取得用ではありません。

クローズはローカルユーザーの承認後、固定された `PATCH` 操作で `state=closed` に変更します。すでにクローズ済みである必要はありません。マージ、ブランチ削除、コメントやレビューの追加、任意 API の呼び出し、別リポジトリの指定はできません。

承認前に worktree と作業ツリー固有・リポジトリ共通の Git 管理ディレクトリを固定し、承認後も同じファイルディスクリプタを再利用します。認証情報はリポジトリ固有の `gh-git` 設定から取得し、グローバルアカウントは切り替えません。

この契約はトークンや任意の API リクエスト本文を公開しません。

実行環境で新しいツールが利用可能だと判断するには、ソースとビルドの確認に加え、再ビルドした実行環境でのカナリア確認が必要です。記録済みの証拠が得られるまで、fixture を使うクローズ試験は保留です。

English: [GitHub pull-request broker](../github-pr-broker.md)
