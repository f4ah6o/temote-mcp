# 不要なセッションが通常 upgrade を妨げる場合の復旧手順を短くする

Status: open
Model: unknown
Created: 2026-10-09
Updated: 2026-10-09
Branch: docs/20261009-upgrade-blocker-retirement

## 概要

再起動コンテキストの差分で通常の `temote upgrade` が止まったとき、利用者が不要と指定したセッションだけを安全に通常停止し、更新へ戻る手順と診断を改善する。

## 背景

2026-10-09 の Mac 実機更新で、インストール済み CLI は `2026.10.2` だったが、稼働 supervisor は `2026.9.24` だった。通常の `temote upgrade` は `dogfood-20261005` の `PATH`、`mbt` の `LANG` / `PATH` に関する restart context blocker で終了コード1となり、handoff に進まなかった。環境変数の値は取得・再現していない。

対象2件が不要だと明示された後、正規CLIの通常停止と通常 upgrade で復旧できた。この追加判断と複数の手動診断が、実機を更新する目的に対する摩擦になった。

## 問題

- blocker の理由だけでは、不要なセッションを選択して停止し、通常更新へ戻る安全な経路が分かりにくい。
- 旧 supervisor は新CLIの read-only session diagnostics に未対応だった。通常の保存先で `mbt` の metadata が欠けていてもソケットは supervisor が所有しており、保存ファイルの不在だけでは停止可能性を判断できなかった。
- 通常停止の戻り値、handoff 成功、Link の継続、リモート接続検証の実施可否を分けて報告する必要がある。診断を呼ぶCLIの設定不足を、稼働サービスの未設定と同一視してはいけない。

## 目標

明示的に選択・承認された不要セッションを停止して更新する操作を、非秘密の診断と既存の正規 lifecycle 操作で短く完了できるようにする。

## 対象外

未承認のセッション停止、環境変数の保存値の抽出や再現、`--force` を既定にする変更、手動の metadata 修復・削除、無関係な作業の停止、新しい認証や権限の設定、Link の独立再起動は含めない。

## 提案する方針

まず通常 upgrade の blocker に、対象ID、非秘密の理由とキー名、検証済み owner/state、次に使う通常停止手順を示す。既存 read-only診断が未対応の場合は未対応と明示し、診断のために legacy maintenance を暗黙実行しない。

利用者が指定したIDとセッションの実体を再確認してから、既存の `session stop` を使用する。既に与えられた同じ対象への停止許可は再利用し、セッションの実体や範囲が変わった場合は停止しない。停止の途中失敗では正常停止済み・未確認を分けて保持し、強制終了や記録削除で迂回しない。全対象の通常停止成功後に通常 upgrade を行い、CLI・supervisor・Link・接続の確認結果を別々に返す。具体的なCLI構文は実装時に既存 lifecycle 契約と合わせて決める。

## 受け入れ条件

- [ ] blocker の表示には秘密値を含めず、対象ID、理由、許可されたキー名と通常停止による復旧経路を含める。
- [ ] 対象は明示的に選択・承認されたIDだけで、所有者不明・実体変更時には停止しない。
- [ ] 通常停止は保存された成果物と durable session metadata を保持し、対象外セッション・プロセス・リポジトリを変更しない。
- [ ] metadata 欠落・停止失敗・部分成功を区別し、強制終了や手動修復を暗黙実行しない。
- [ ] 全対象の通常停止成功後だけ通常 upgrade へ戻り、実際の終了コードと更新後 runtime version を返す。
- [ ] Link のプロセス継続やローカル generation と、リモート lease / 接続の検証結果を分け、未検証を成功扱いしない。

## テスト計画

対象選択、所有者やセッション実体の変更、metadata 欠落、通常停止の部分失敗、全対象成功後の再preflight、秘密値の非表示を回帰ケースとして固定する。旧 supervisor と新CLIの実機・プロセス境界で、未対応の read-only診断が暗黙の maintenance に変わらないことと、通常停止・handoff・保存状態を確認する。実装を変更する際は AGENTS.md の対象チェックを実行する。

## リスク

PIDやセッションIDの再利用、停止中の他操作、通常停止が所有する子プロセスの終了によって、古い診断だけで停止対象を選ぶと利用中の作業へ影響する。所有者とセッション実体の確認を操作直前に行う必要がある。ローカル Link 記録は、リモート lease の検証を代替しない。

## 変更履歴

現時点は観測と課題の記録のみで、製品コード・CLI仕様の変更はない。実装時には `CHANGES.md` と狭い範囲の運用ドキュメントへの影響を判断する。

## 注記

- 2026-10-09 02:51:46 UTC: `temote session stop dogfood-20261005` は終了コード0、`stopped` / PIDなしを返した。
- 2026-10-09 02:52:06 UTC: `temote session stop mbt` も終了コード0、`stopped` / PIDなしを返した。保存先での metadata 不在を理由に手動修復はしていない。
- 2026-10-09 02:52:48 UTC: 続く通常 `temote upgrade` は終了コード0で `2026.9.24 -> 2026.10.2`、restored 0件を返した。`--force` は使っていない。
- 2026-10-09: 更新後の read-only upgrade preview は source / target とも `2026.10.2`、blocker 0、helper compatible、handoff不要。停止した2件の記録は保持され、ソケットは不在。supervisor は同じPIDで更新され、Linkと親の `op` のPIDは継続した。
- 2026-10-09: 既定CLIコンテキストにはFabric host ID / URL / host tokenの設定がなく、`fabric status` のリモート項目は `not_checked`。ローカルLink generation記録は存在したが、Link実バージョン・リモートlease・接続の成功は未検証。認証情報の取得や新規設定は行っていない。
- 2026-10-09: 後続の非秘密launch診断で、既存Linkは `op run --env-file` 配下の `gateway-agent` で host `FUJITAnoMac-mini.local` を指定していた。env-fileは所有者限定FIFOで、本文は取得していない。既定CLIの設定不足はこのサービスの未設定を示さない。
- 2026-10-09: LinkのOS画像は10月1日から継続し、現行インストール済みバイナリとは別inodeだった。OSの版表示 `0` はTemoteのCalVerを確定する証拠ではない。Fabric登録の `runtime_version` はsupervisorの版を表すため、Link自身の実行版と混同しない。
- 2026-10-09: 現在の正式な `op whoami` は認証利用不可。非認証healthはAccessから401、既存Chromeでのhealth表示もAccessログイン画面。ログインや新しい供給、Link再起動は行っておらず、稼働Linkの既存認証を別contextへコピーしていない。
- Branch は課題記録だけを隔離する保存用branch。runtime改善コードの実装は開始していない。

Related:
- [metadata 欠落と Link PATH の既存課題](../doing/20261005-dogfood-supervisor-metadata-and-link-path.md)
- [復元不能セッションの明示的な force 停止の既存実装](../done/20260928-upgrade-force-blocked-sessions.md)
- [runtime 観測整合の既存実装](../done/20260916-upgrade-runtime-observation-consistency.md)
