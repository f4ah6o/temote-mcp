# 旧 supervisor からの upgrade で helper 世代判定が unavailable になる

Status: open
Model: openai/gpt-6-sol
Created: 2026-09-24
Updated: 2026-09-24
Branch: fix/20260924-upgrade-legacy-helper-preflight

## 概要

稼働中の旧 supervisor が helper 世代を報告しない場合も、インストール済み helper を安全に検査して upgrade を継続できるようにする。修正版の配布と稼働ホストでの検証まで追跡する。

## 背景

- `issues/done/20260916-upgrade-helper-generation-preflight.md` で helper 世代の検査を追加した。旧 supervisor の dry-run 応答には `helper_generation` が存在しない。
- 2026-09-24、ホスト `ms-01-alpha` で `temote-mcp upgrade --dry-run`（稼働 2026.9.10 → インストール済み 2026.9.15）を実行した。最初は消失した workspace を持つ5件の degraded session が `session_not_restorable` となり、`upgrade` は `upgrade is blocked by 5 session(s)` で終了した。
- 当該5件は `session info` とディレクトリの不在を確認して停止済み。再度の dry-run は `blocked_session_count: 0`、`planned_session_count: 50`、`direct_ingress_blocked: false` だが、`helper_generation: unavailable` のまま。helper バイナリはインストール済みで `--capabilities` は `policy_schema: 1` を報告する。実際の handoff は未実施。

## 問題

新 CLI は旧 supervisor の dry-run 応答に helper 判定がない場合 `unavailable` と解釈し、インストール済み helper が互換であっても upgrade を拒否する。`upgrade --dry-run` の `compatible: true` はこの gate の通過を意味しない。

## 目標

旧 supervisor と互換プロトコルでの handoff 時に、helper を検査できれば正常に upgrade でき、検査失敗や非互換時は handoff 前に拒否する。対象ホストで50件の稼働セッションを保護したまま復元結果を確認する。

## 対象外

停止済みの5セッションの workspace 再作成、手動 supervisor restart による一括停止、helper の互換性検査の省略。

## 提案する方針

`src/session_control.rs` の `upgrade_preflight_with_force` で旧応答に判定フィールドがない場合のみ、インストール済み executable の隣の helper を既存の bounded metadata / capability 検査で分類する。明示的な `unavailable`、不正な値、helper の欠落は fail closed とする。作業ツリーにはこの修正とテスト、`docs/managed-sessions*.md` の説明が未コミットで存在するため、取り込む際は差分を確認する。リリース後、修正版 CLI を導入して dry-run と handoff の結果を確認する。

## 受け入れ条件

- [ ] 旧 supervisor 応答に `helper_generation` がなく、同梱 helper の schema が一致する場合に dry-run が `compatible` を返す。
- [ ] 旧応答でも helper 欠落・非互換を拒否し、新 supervisor の明示的な `unavailable` を上書きしない。
- [ ] 修正版を導入したホストで dry-run の `blocked_session_count: 0` と `helper_generation: compatible` を確認し、実際の upgrade で計画したセッションの復元と ingress の health を確認する。

## テスト計画

- `cargo test legacy_upgrade_preview_checks_installed_helper_locally`
- `cargo fmt --all -- --check`、`cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`
- 実ホストの `temote-mcp upgrade --dry-run` → `temote-mcp upgrade` → `temote-mcp session list` / ingress health 確認。作業ツリーでは targeted test、fmt、clippy、no-default-features check、diff check、および OpenCode 実サービス実行ファイルを PATH から外した `cargo test --all-targets --all-features -- --test-threads=1` と gateway の `npm test` を通過済み。実ホスト handoff は未実施。

## リスク

旧 supervisor は helper gate を実装していないため、CLI 側で必ず同梱 helper を検査してから変更を開始する。リポジトリの baseline version はリリース版と異なるので、バージョンを手動で進めず CalVer のリリース手順に従う。稼働中50セッションの手動再起動は restart context の喪失につながり得る。

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- 旧 supervisor からの upgrade で helper 世代が報告されない場合も、インストール済み helper を検査して互換なら handoff を継続できるようにした。

## 注記

関連: `issues/done/20260916-upgrade-helper-generation-preflight.md`、`docs/managed-sessions.ja.md`。
