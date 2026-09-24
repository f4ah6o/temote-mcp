# OpenCode serve で CLI の provider 設定と認証を安全に共有する

Status: done
Model: openai/gpt-6-sol
Created: 2026-09-24
Updated: 2026-09-24
Branch: feat/20260924-opencode-serve-shared-provider

## 概要

Temote MCP が管理する task ごとの `opencode serve` で、ホストの OpenCode CLI と同じ provider/model 設定と接続済みアカウントを利用できるようにする。

## 背景

`opencode_task_*` は task ごとに loopback の serve child と専用データディレクトリを作成する（`src/opencode_server.rs`）。
OpenCode V2 の provider 設定はグローバル `~/.config/opencode/opencode.json(c)` に、`auth login` / `/connect` の保存済み認証はユーザーの SQLite データベースにある（[V2 configuration](https://opencode.ai/v2/docs/config)、[provider accounts](https://opencode.ai/v2/docs/cli/providers)）。
関連: [server backend の設計](../open/20260922-agent-server-backends-cli-deprecation.md)。

## 問題

serve child は `XDG_CONFIG_HOME` を引き継ぐ一方、`XDG_DATA_HOME` を task ごとに隔離し、旧 `auth.json` のみコピーする。
V2 の SQLite 保存アカウントは渡らず、provider 環境変数も child の allowlist に含まれないため、CLI で接続済みの provider が task 実行時には未認証になる。
ホスト DB を child と共有すると task 間の session/state 隔離が失われる。

## 目標

ホストと共通のグローバル provider/model 定義を読み、Temote の権限上書きを維持しつつ、接続済み provider の認証を task 専用 state に必要最小限だけ安全に取り込む。
V1 の `auth.json` と V2 の保存済み認証の両方を対象とする。

## 対象外

- task 間の OpenCode session/history の共有、ホスト DB 全体のコピーまたは DB パスの共有。
- 公開 MCP tool からの任意の認証値・環境変数・設定ファイル指定。
- `local_agent_run` / Codex の認証仕様変更。

## 提案する方針

`src/opencode_server.rs` の spawn 前に OpenCode のデータパスと設定パスをホスト側で解決する。
V2 の資格情報は保存形式・移行互換を調査したうえで、秘密値をログや tool output に出さず task 専用 state に必要なアカウント情報だけを取り込む。
実装が特定の SQLite schema に依存する場合は、未対応 schema で fail closed し、バージョンとテストで互換範囲を明示する。
V1 の `auth.json` と子プロセスの権限設定の回帰を防ぎ、既存のグローバル設定から provider/model 定義が読めることを確認する。

## 受け入れ条件

- [x] CLI のグローバル provider/model 定義を child が読み、接続済み V2 アカウントを task 専用 DB で認識できる。
- [x] V1 の `auth.json` も引き続き利用できる。
- [x] child は task 専用 state のままで、ホスト/他 task の session・history・credential DB 本体を共有しない。
- [x] 認証値が task record、audit、approval summary、エラー、通常の tool output に出ない。
- [x] 未対応・破損・symlink の認証ソースで安全に失敗し、適切なテストと運用手順を記録する。

## テスト計画

- 一時 HOME/XDG ディレクトリに偽の資格情報と別 task の session を配置し、task 間隔離、資格情報取り込み、ファイル権限、secret 非露出を検証する。
- インストール済みの OpenCode V2 で実際の保存形式と serve 起動を確認する（認証キーや DB は表示しない）。
- `cargo fmt --all -- --check`、`cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`。

## リスク

OpenCode V2 の内部 DB schema や認証更新方法は変更され得る。資格情報の複製は token refresh の同期と削除時の失効にも注意する。グローバル設定にある別の project 固有値や child の権限設定が混ざらないようにする。

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- `opencode_task_*` の task 専用 serve child が、CLI と共通の provider 設定・保存済み認証を安全に利用できるようにした。

## 注記

OpenCode V2 の認証保存形式と task データ隔離の両立を実装前に確認する。
- 2026-09-24: 受け入れ条件と認証境界を確定した
- 2026-09-24: OpenCode V2 資格情報の task 専用取り込みに着手した
- 2026-09-24: OpenCode 2.0.15 の実 DB schema を確認。実ホスト DB は 1.4GB のため、全量 snapshot は使わず、schema と credential/migration 行のみを task DB へ転記。実ホスト DB からの private copy を `opencode auth list --standalone` が認識することを確認した。実 model turn は upstream free-tier の HTTP 403 により利用可否を判定できない（認証取り込みの検証には含めない）。`cargo test` 全件は変更途中で PASS、最終差分の format/clippy/no-default-features/diff checks は PASS。live V2 contract test は provider 側の 403 で失敗した。
- 2026-09-24: V2 保存資格情報の限定転記、隔離と秘密値保護、旧 auth.json 維持を検証した
- 2026-09-24: 最終検証では live V2 model test が provider の 403、別の全件実行で無関係な upgrade snapshot test が `Text file busy` で一度失敗。upgrade snapshot test は単独再実行で PASS。資格情報 seed の単体テストと実ホスト DB を使う CLI 接続一覧確認は PASS。
