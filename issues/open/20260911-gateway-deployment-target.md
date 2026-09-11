# Gateway の deployment target を明示・文書化する

Status: open / Slice A (docs decision table + deterministic docs check) implemented in the current worktree; optional preflight and live acceptance not started
Created: 2026-09-11
Updated: 2026-09-11
Priority: P1 operational correctness

## 概要

`workers_dev = false` の Gateway Worker について、custom domain と既存 DNS hostname への Worker route の選択、deploy 時の target 指定、target の検証方法を docs と必要最小限の運用 tooling で明確にする。

この issue は **Gateway の routing/protocol 実装ではなく deployment correctness** を扱う。最初の実装スライスでは実 Cloudflare 設定を変更しない。

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
- 実 Cloudflare account の route/domain を repository test から自動変更すること

## 運用契約

Gateway を公開する operator は、deploy 前に次のどちらか一方を明示的に選ぶ。

### A. Custom domain binding

- Cloudflare Worker custom domain を明示的に作成する。
- 対象 hostname 全体を Access で保護する。
- MCP client 用 Managed OAuth と host-agent 用 Access service-token policy を混同しない。
- deploy 後に domain binding が intended Worker/version を指すことを read-only で確認する。

### B. Existing DNS + Worker route

- 既存 DNS record を削除しない。
- exact hostname/pattern を `wrangler deploy --routes '<hostname>/*'` などの明示 target として渡す。
- 対象 hostname 全体を Access で保護する。
- deploy 後に route が intended Worker/version を指すことを read-only で確認する。

どちらの方式でも `workers_dev = false` を維持し、target 未指定の deploy を成功扱いしない。

## Secret / variable boundary

- dashboard-managed non-secret variables を保持する deploy では `--keep-vars` を使う。
- Worker secrets は `wrangler secret` / Cloudflare の secret 管理に残し、issue/docs/log に値を保存しない。
- Access service-token credential と Gateway host token は別の credential boundary として扱う。
- verification は secret の「存在/認証結果」を扱っても値を表示しない。

## Executable implementation slices

### Slice A — docs + deterministic local checks

- `docs/gateway.md` / `docs/gateway.ja.md` に custom domain と existing DNS + Worker route の decision table を追加する。
- `workers_dev = false` の前提、Access/OAuth/service-token 境界、`--keep-vars`、secret 管理、rollback を明記する。
- `No targets deployed` を成功扱いしない deploy checklist を追加する。
- exact hostname/pattern 以外を変更しない rollback 手順を書く。
- docs/example の決定論的 check を追加する。
- **実 Cloudflare route/domain は変更しない。**

**Done when:** operator が docs だけで target を選択し、dry-run/deploy/verify/rollback の順序を再現できる。

### Slice B — optional target preflight

Slice A の後、手作業だけでは target 欠落を十分検出できない場合にのみ実装する。

- repository script または narrow CLI preflight として実装する。
- `workers_dev=false` かつ target 未指定を明示 failure にする。
- intended gateway hostname と configured route/domain の不一致を non-secret に報告する。
- Cloudflare credential がない場合は remote verification を `unknown/not checked` とし、ready と偽装しない。
- DNS/Access/route の mutation は行わない。

**Done when:** target 未指定・hostname 不一致・remote 未確認を distinct status で返せる。

### Slice C — live deployment acceptance

- 実 account で選択した方式を一度適用する。
- deploy output と route/domain の read-only verification を記録する。
- public hostname の疎通、Access、host-agent の成立確認は live acceptance matrix に evidence を残す。
- secret 値は保存しない。

**Done when:** repository-local implementation issue と credential-dependent live evidence が分離されている。

## 受け入れ条件

- [ ] custom domain と既存 DNS + Worker route の違い、選択条件、必要な Access 設定が英日 docs に記載される。
- [ ] `workers_dev = false` で target 未設定の場合に、deploy 手順または preflight が `No targets deployed` を見逃さない。
- [ ] 既存 DNS record を削除せずに route を割り当てる実行例と、deploy 後の route/domain verification が記載される。
- [ ] `--keep-vars`、Worker secrets、Access service token、host token の管理境界が混同されない。
- [ ] 意図しない workers.dev 公開を有効化しない。
- [ ] docs または tooling の変更に対する決定論的な test/check が追加される。
- [ ] 実環境でしか確認できない項目は `20260908-live-acceptance-matrix.md` に evidence として分離される。

## テスト計画

Repository-local:

- docs/example の deterministic check
- preflight を追加した場合は fake wrangler/config を使った target-present / target-missing / mismatch / remote-unknown tests
- `git diff --check`

Live acceptance（実装完了の repository-local gate とは分離）:

- `npx wrangler deploy --dry-run --keep-vars --routes '<hostname>/*'`
- 実 Cloudflare account で target 一覧を read-only 確認し、対象 hostname が intended Worker に割り当てられていることを確認する
- 選択した deployment 方式について deploy output と公開 hostname の疎通を確認する

## リスク

同一 hostname に direct Tunnel と Worker route が共存すると、route の追加・削除によって接続先が切り替わる。変更対象を exact hostname/pattern に限定し、DNS・Access・Tunnel の各状態を個別に確認できる rollback 手順を docs に含める必要がある。target 検証用 API credential は秘密値をログや issue に保存しない。

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Gateway Worker の deployment target 選択、既存 DNS route、custom domain の運用手順を明確化。

## 注記

- 2026-09-11: `localmcp.obr-grp.com/*` への Worker route は direct Temote 復旧のため削除した。DNS record、Access application、Tunnel は削除していない。
- 2026-09-11: `workers_dev = false` の route/domain 未設定 deploy で `No targets deployed` が発生したため、再発防止の issue として記録した。

## Recommended next slice

**Slice A only.** 実 Cloudflare mutation を伴わず、deployment target の選択・検証契約を先に固定する。

## Implementation notes (2026-09-11)

Slice A is implemented in the current worktree:

- `docs/gateway.md` / `docs/gateway.ja.md` now contain the custom-domain vs existing-DNS + Worker-route decision table, the `workers_dev = false` requirement, the `No targets deployed` failure rule, `--keep-vars` and secret-boundary guidance, read-only post-deploy verification (`wrangler deployments status`, `/healthz` service identity), and exact-pattern rollback that leaves DNS, Access, and Tunnel untouched.
- `tests/gateway_deployment_docs.rs` deterministically checks both documents for the required operator contract and rejects any `workers_dev = true` guidance.

No Cloudflare route, DNS, Access, or Tunnel state was changed.
