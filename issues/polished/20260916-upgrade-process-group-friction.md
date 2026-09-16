# `temote-mcp upgrade` の helper と direct ingress handoff を安定化する

Status: polished
Model: deepseek-v4.1-flash
Created: 2026-09-16
Updated: 2026-09-16
Branch: main

## 概要

Linux ホストで crates.io の `temote-mcp 2026.9.9` をインストールし、稼働中の `2026.9.7` supervisor を `temote-mcp upgrade` で更新した際、supervisor の same-PID handoff 自体は成功したが、helper、direct ingress、実行中の Codex client の世代切り替えが一連の運用として安定していなかった。

インストール直後の通常 sandbox 実行失敗、direct ingress の detached child が呼び出し元の実行環境終了時に回収される事象、実行環境による dry-run 観測差を解消し、更新完了を利用者が再現可能な証拠で確認できるようにする。

## 背景

既存の [`temote-mcp upgrade` handoff](../done/20260902-zero-downtime-supervisor-upgrade.md) は、target binary の互換性検証、active session の drain/restore、same-PID `exec`、direct ingress の health verification、Codex plugin reconciliation を実装済みである。

運用手順は [`docs/managed-sessions.md`](../../docs/managed-sessions.md) と [`skills/temote-mcp/SKILL.md`](../../skills/temote-mcp/SKILL.md) にあり、replacement binary のインストール後に `upgrade --dry-run`、`upgrade` を実行する契約になっている。

remote MCP client 自身が更新対象 ingress を呼び出す場合の durable coordinator と reconnect は、別の [`client-safe upgrade` issue](../doing/20260908-07-client-safe-upgrade-reconnect.md) で扱う。本 issue は、外部の local CLI または agent 実行環境から行う既存の local upgrade の運用安定性を扱う。

## 問題

2026-09-16 の Linux ホストで、初期状態は `temote-mcp 2026.9.7`、同一 supervisor の active session は12件だった。次の摩擦を観測した。

1. `cargo install temote-mcp --version 2026.9.9 --locked` は `temote-mcp`、`temote-linux-sandbox`、`temote-onepassword-sdk` を同時に置き換えた。旧 supervisor が稼働したままの短い期間に通常の `execute` を呼ぶと、`temote-linux-sandbox: invalid Linux sandbox helper arguments` で失敗した。
2. Temote の host command と local shell から同じ `upgrade --dry-run` を確認したところ、前者は direct ingress を inactive と報告し、後者は `127.0.0.1:8791` の Cloudflare ingress を source `2026.9.7` として active/healthy と報告した。runtime directory または呼び出し元の process namespace を含む観測条件が揃っていない。
3. `upgrade` は supervisor handoff（`2026.9.7 -> 2026.9.9`、PID維持、12 session restore）と direct ingress restart の `health=healthy` を報告した。しかしコマンド終了後に ingress process が残らず、port 8791 は接続拒否、PID file は stale と判定された。`setsid` で同じ durable recipe を起動し直すと ingress は維持され、`upgrade --dry-run` は source/target `2026.9.9`、`untouched`、healthy になった。
4. plugin は `2026.9.9` に reconcile されたが、実行中の Codex client には再起動要求が返った。client を再起動するまで、既存 MCP bridge からの通常 `execute` は新 helper 世代に追随せず、更新完了の確認も分かりにくい。

現行実装の health probe は upgrade コマンドが終了する前の状態を確認するため、呼び出し元の PTY/job/process-group cleanup によって detached child が後から終了する場合を捕捉できない。また binary と helper の置換順序、runtime state の観測主体、client plugin reload の境界が operator-facing contract として一つに整理されていない。

## 目標

- replacement binary と sandbox helper の世代を安全に切り替え、supported upgrade sequence の途中で通常 session の sandbox 実行が不意に壊れないようにする。
- direct ingress の restart process を呼び出し元の PTY、job、process group の寿命から独立させるか、独立できない場合は成功を報告せず deterministic に失敗させる。
- `upgrade` 終了後も target version の ingress process、owner-only PID/state、health endpoint が維持されることを証明する。
- local shell と Temote 経由の dry-run が同じ runtime state を観測し、差異がある場合は秘密値を含まない診断を返す。
- Codex plugin の reconcile と既存 client の restart/reconnect 要求を、更新成功・未完了の判定と混同しない形で明示する。

## 対象外

- remote MCP から ingress 自身を更新する durable coordinator、transport commit barrier、reconnect API。これらは `issues/doing/20260908-07-client-safe-upgrade-reconnect.md` の対象とする。
- `--yolo`、sandbox、permission、session root の境界を緩める回避策。
- package version metadata の手動更新。CalVer の version bump は release workflow が所有する。
- 既存の unrelated な working-tree changes、`.tmp/`、`.wt/`、既存の session metadata の整理。

## 提案する方針

- supervisor、sandbox helper、付随 helper を世代付きの installation bundle として扱い、active supervisor が使用中の helper と replacement binary の世代不一致を preflight で検出する。置換順序または互換 shim を設計し、通常操作を壊す期間をなくすか、handoff まで deterministic に拒否する。
- direct ingress の restart ownership を明確化する。`upgrade` の子 process を安定した supervisor/process session に移す場合は、親 command の終了後も PID lock、runtime state、listener が維持されることを bounded window 後に再検証する。維持できない場合は stale state を成功として残さず、rollback/recovery 情報を返す。
- dry-run と apply が同じ runtime directory、PID lock、state schema、host identity を使うようにし、実行主体が異なる場合の差異を非秘密の `runtime_dir`、source/target、PID、health、action として報告する。
- plugin reconciliation は binary/supervisor/ingress の成功状態と分離して記録し、client restart が必要な場合は、旧 client が保持する tool inventory と新 plugin の関係を明示する。
- direct ingress の process-group cleanup、helper 世代不一致、runtime state の stale 化、client restart 後の通常 execute を Linux の integration/E2E test で固定する。既存の session restore、secret-free state、same-PID handoff の契約は維持する。

## 受け入れ条件

- [ ] supported upgrade sequence で replacement binary と helper の世代不一致が通常 sandbox 実行の予期しない失敗にならず、互換性がない場合は destructive action 前に明示的に停止する。
- [ ] PTY、background job、process-group cleanup を伴う実行環境でも、`upgrade` 終了後に target version の direct ingress process と owner-only PID/state が存続する。
- [ ] apply の成功は、コマンド終了後にも listener と `/healthz` が target version で healthy であること、および stale PID/state がないことを含む。
- [ ] local shell と Temote 経由の `upgrade --dry-run` が同一の active ingress を同じ source version/action として分類するか、観測差の原因を非秘密に報告する。
- [ ] supervisor の same-PID handoff、計画された active session の restore、permission mode、session socket probe が既存契約どおり維持される。
- [ ] plugin reconcile と client restart/reconnect の必要性が更新結果から判別でき、fresh client の通常 sandbox `execute` が target helper 世代で成功する。
- [ ] credentials、token、環境変数の secret value が state、ログ、approval summary、Issue、通常 tool output に出ない。

## テスト計画

- `cargo test` に helper/supervisor 世代切り替え、direct ingress の parent process-group 終了、stale PID/state、restart 後の health verification の回帰テストを追加する。
- Linux の実環境で、local shell と Temote 経由の `upgrade --dry-run`、`upgrade`、コマンド終了後の listener、`/healthz`、fresh client の `execute` を確認する。
- `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`、issues CLI の `validate` を実行する。runtime/lifecycle または shared protocol を変更した場合は `(cd gateway && npm test)` も実行する。
- plugin reconcile 後に既存 Codex client と fresh client の tool inventory がそれぞれどの世代を読み込んでいるかを、秘密値なしで確認する。

## リスク

process group の変更は ingress の孤児 process、二重起動、port collision、Cloudflare Tunnel の重複接続を生む可能性がある。PID lock、process name、host identity、runtime state の一致を再確認し、異なる process を停止しないこと。

install/handoff の順序変更は supervisor と helper の protocol 互換性を壊す可能性がある。既存の capability/schema check と same-PID restore を先に保護し、rollback 時にも credential value を再構成しない。

client restart を自動化すると利用中の会話や MCP connection を切断する可能性があるため、server 側の更新成功と client 側の再接続完了を別状態として扱う。

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- `temote-mcp upgrade` が helper、supervisor、direct ingress、Codex plugin の世代切り替えを実行環境に依存せず完了できるようになったこと、および client restart/reconnect の必要条件を明示する。

## 注記

- 2026-09-16: `cargo install temote-mcp --version 2026.9.9 --locked` は成功した。Cargo は yanked dependency `chacha20 v0.10.1` を警告したが、package install 自体は完了した。
- 2026-09-16: 実稼働 supervisor は same-PID handoff で `2026.9.7 -> 2026.9.9` に更新され、active 12 session は restore 後も `active` だった。作業ツリーの既存変更はこの検証で変更していない。
- 2026-09-16: `upgrade` の direct ingress restart は一度 `health=healthy` を返した後、呼び出し元 command の終了後に process が残らず、PID 287182 の stale state と port 8791 の接続拒否を観測した。`setsid` で同じ非秘密 restart recipe を起動すると PID 288826 で維持され、`upgrade --dry-run` は target `2026.9.9` / `untouched` / healthy、`/healthz` は `status=ok` になった。
- 2026-09-16: `upgrade` は `/home/hirohito-fujita/.codex/plugins/cache/debug/temote-mcp/2026.9.9` への plugin install と既存 Codex client の restart 要求を返した。現在の client session をこの作業中に再起動していないため、旧 bridge 経由の通常 `execute` は helper argument error のまま残っている。
- 2026-09-16: 関連 issue は `issues/done/20260902-zero-downtime-supervisor-upgrade.md` と `issues/doing/20260908-07-client-safe-upgrade-reconnect.md`。前者は local supervisor handoff の実装済み契約、後者は remote ingress reconnect の未実装契約を追跡する。

## Triage note

- 2026-09-16: 観測された摩擦、受け入れ条件、テスト計画が揃っており実装に着手できるため `ready` と判定し、`issues/open/` から `issues/polished/` へ移動した。実装時は (1) replacement binary と sandbox helper の世代切り替え、(2) direct ingress restart の process-group ownership、(3) dry-run/apply の runtime 観測一致、(4) plugin reconcile と client restart の分離、の slice 分割を推奨する。live 実機確認は `20260908-live-acceptance-matrix.md` 側で扱う。
