# upgrade --dry-run の NG session 一覧と upgrade --force による復元不能 session の切り離し

Status: open — implementation in draft PR
Created: 2026-09-28
Updated: 2026-09-28

## 概要

`temote-mcp upgrade --dry-run` で upgrade を阻害している session の ID と理由を一覧取得できるようにし、`temote-mcp upgrade --force` で復元不可能な session を停止して handoff を続行できるようにする。

## 背景

- `issues/open/20260924-upgrade-legacy-helper-preflight.md` の実ホスト検証で、`blocked_session_count: 5` とだけ報告され、どの session が NG かは `session list` / `session info` を個別に調べて特定する必要があった。
- supervisor の preview は既に `blocked_sessions`(session_id + reason)を返しているが、`RemoteUpgradePreflight` が count と汎用の `blocker_reasons`(`session_not_restorable` の繰り返し)だけを残して詳細を捨てている。
- `upgrade` は blocked session が1件でもあれば中断する。NG session を手動で個別に停止しない限り upgrade できない。

## 問題

1. `upgrade --dry-run` の出力から NG session の一覧が取れない。
2. `upgrade --force` を指定しても blocked session があると中断し、復元不能な session を諦めて upgrade する方法がない。

## 目標

- dry-run の JSON に `blocked_sessions`(`session_id` と `reason`)を含める。
- `upgrade --force` は NG session を停止(stop)してから preflight を再評価し、残りの session を通常の handoff で復元する。停止した session は復元されず stopped になる。
- control protocol / lifecycle schema / direct ingress / sandbox helper の互換性ゲートは `--force` でも回避しない。

## 対象外

- 直接 HTTP `upgrade_apply` の force 対応。承認ベースのリモート経路は厳格なままとする。
- NG session の metadata 削除や `session forget` の自動化。停止した session の metadata は既存の retention に委ねる。
- `handoff_required == false` の dry-run(version 一致かつ force なし)で session 個別検査を行わない現行仕様の変更。same-version で blocker 一覧が必要な場合は `upgrade --dry-run --force` を使う。

## 提案する方針

- `RemoteUpgradePreflight` に `blocked_sessions: Vec<UpgradeSessionBlocker>` を追加し、supervisor preview の session_id / reason をそのまま公開する。`blocked_session_count` / `blocker_reasons` は互換のため残す。
- `session_control::upgrade` で blocked session が残っている場合、`--force` 指定時に限り各 session に `ControlRequest::Stop` を送って停止し、preflight を再実行してから従来の gate を通す。個別の stop 失敗は警告を出し、再 preflight の blocked 数で最終判定する。client 側の `Stop` で完結するため、稼働中の旧 supervisor に対しても有効で、server 側の plan/drain/rollback 経路は変更しない。

## 受け入れ条件

- [ ] `upgrade --dry-run` の JSON 出力に NG session の `session_id` と `reason` が `blocked_sessions` として含まれる。
- [ ] `upgrade`(force なし)は blocked session があると従来通り中断する。
- [ ] `upgrade --force` は NG session を停止し、残りの session を handoff で復元して完了を報告する。停止した件数を完了メッセージで報告する。
- [ ] `--force` でも ingress / helper / protocol の非互換は拒否される。

## テスト計画

- `cargo fmt --all -- --check`、`cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`
- process-boundary E2E(`--ignored`): supervisor + workspace を削除した session で dry-run の blocked list、`--force` による session 停止と handoff 成功を確認。

## リスク

- `--force` は NG session の実行中の作業を失わせる。dry-run の一覧で対象を確認してから使うことを docs / skill に明記する。reason には secret の値を含めない(既存の preview blocker 契約を維持する)。

## 変更履歴

`CHANGES.md` impact: yes

項目案:

- `upgrade --dry-run` が復元不能 session の `session_id` と理由を `blocked_sessions` で一覧し、`upgrade --force` がそれらを停止して handoff を続行するようにした。
