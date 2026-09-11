# Gateway の deployment target を明示・文書化する

Status: open
Model: unknown
Created: 2026-09-11
Updated: 2026-09-11
Branch: feat/developer-execution-broker-agent

## 概要

`workers_dev = false` の Gateway Worker について、custom domain と既存 DNS hostname への Worker route の選択、deploy 時の target 指定、target の検証方法を docs と運用 tooling で明確にする。

## 背景

Gateway の基本設定と host-agent 要件は [`docs/gateway.md`](../../docs/gateway.md) と [`docs/gateway.ja.md`](../../docs/gateway.ja.md) に記録されている。現在の設定は `workers_dev = false` で、Worker の route/domain を `gateway/wrangler.toml` に宣言していない。

既存 DNS を維持したまま hostname を Worker に割り当てる場合は、Wrangler deploy に route target を明示する必要がある。一方、custom domain を作成する場合は Cloudflare 側の domain binding と Access application を別途構成する必要がある。

## 問題

target を設定ファイルまたは deploy 手順で明示しないまま `wrangler deploy` を実行すると、Worker version は upload されても `No targets deployed` となり、期待する公開 hostname は direct origin または未割り当てのままになる。

custom domain と Worker route の違い、既存 DNS record を削除せずに route を上書きする手順、`--keep-vars` が必要な dashboard-managed variables の扱い、deploy 後の route verification が一つの手順として整理されていない。

## 目標

Gateway の公開 target を再現可能な形で選択・deploy・検証できるようにし、target 未設定や意図しない `workers.dev` 公開を operator が早期に検出できるようにする。

## 対象外

- Cloudflare Access policy の自動作成・自動変更
- DNS record の削除または自動移行
- Gateway の MCP protocol、Durable Object、host-agent routing semantics の変更
- `workers_dev = true` への変更
- Gateway federation readiness の end-to-end 診断（`20260911-gateway-doctor-readiness.md` で扱う）

## 提案する方針

1. `docs/gateway.md` と `docs/gateway.ja.md` に、次の2方式を分けて記載する。
   - Cloudflare custom domain binding を使う方式
   - 既存 DNS hostname を維持し、`wrangler deploy --routes '<hostname>/*'` で Worker route を割り当てる方式
2. 両方式で `workers_dev = false`、hostname 全体の Access 保護、Managed OAuth、host-agent 用 Access service-token policy を必須条件として明記する。
3. dashboard-managed variables を保持する deploy では `--keep-vars` を使い、Worker secret は `wrangler secret` で別管理する手順を固定する。
4. dry-run、deploy output、Cloudflare route/domain の read-only verification を一連の手順にする。既存 DNS record は削除しない。
5. 必要なら `temote-mcp` または repository script に、target がない場合と direct/gateway hostname の不一致を秘密値なしで報告する preflight を追加する。

## 受け入れ条件

- [ ] custom domain と既存 DNS + Worker route の違い、選択条件、必要な Access 設定が英日 docs に記載される。
- [ ] `workers_dev = false` で target 未設定の場合に、deploy 手順または preflight が `No targets deployed` を見逃さない。
- [ ] 既存 DNS record を削除せずに route を割り当てる実行例と、deploy 後の route/domain verification が記載される。
- [ ] `--keep-vars`、Worker secrets、Access service token、host token の管理境界が混同されない。
- [ ] 意図しない workers.dev 公開を有効化しない。
- [ ] docs または tooling の変更に対する決定論的な test/check が追加される。

## テスト計画

- `npx wrangler deploy --dry-run --keep-vars --routes '<hostname>/*'`
- 実 Cloudflare account で target 一覧を read-only 確認し、対象 hostname が intended Worker に割り当てられていることを確認する
- custom domain 方式と route 方式について、deploy output と公開 hostname の疎通を確認する
- `git diff --check`

## リスク

同一 hostname に direct Tunnel と Worker route が共存すると、route の追加・削除によって接続先が切り替わる。変更対象を exact hostname/pattern に限定し、DNS・Access・Tunnel の各状態を個別に確認できる rollback 手順を docs に含める必要がある。target 検証用 API credential は秘密値をログや issue に保存しない。

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Gateway Worker の deployment target 選択、既存 DNS route、custom domain の運用手順を明確化。

## 注記

- 2026-09-11: `localmcp.obr-grp.com/*` への Worker route は direct Temote 復旧のため削除した。DNS record、Access application、Tunnel は削除していない。
- 2026-09-11: `workers_dev = false` の route/domain 未設定 deploy で `No targets deployed` が発生したため、再発防止の issue として記録した。
