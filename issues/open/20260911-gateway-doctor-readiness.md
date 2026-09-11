# Gateway federation の end-to-end readiness 診断を追加する

Status: open
Model: unknown
Created: 2026-09-11
Updated: 2026-09-11
Branch: feat/developer-execution-broker-agent

## 概要

Cloudflare Worker/Durable Objects gateway を利用する前に、host 側設定、local supervisor、Access service token、Worker 到達性、host registration の不足を秘密値を漏らさず判別できる `doctor` 診断を追加する。

## 背景

Gateway の設定要件は [`docs/gateway.md`](../../docs/gateway.md) と [`docs/gateway.ja.md`](../../docs/gateway.ja.md) に記録されている。現行の `temote-mcp doctor` は `TEMOTE_MCP_GATEWAY_HOST_ID` が設定されている場合に、host ID、gateway URL、host token の存在、Access service-token の組み合わせ、local supervisor の control protocol、named-root 数を確認する。

既存の [`issues/open/20260908-live-acceptance-matrix.md`](20260908-live-acceptance-matrix.md) は実 Cloudflare 環境での multi-host acceptance を追跡するが、設定ミスを起動前に切り分ける operator-facing preflight とは目的が異なる。

## 問題

現行診断だけでは、次の状態を local configuration failure と remote deployment failure に分離できない。

- Worker の public route または `/mcp` endpoint が想定 hostname に割り当てられていない。
- Worker の per-host `HOST_TOKENS_JSON` に local `host_id` がない、または token が一致しない。
- Access service token が期限切れ、無効、または host-agent 用 policy に許可されていない。
- `gateway-agent` が起動していない、または Worker の active host lease として登録されていない。
- gateway endpoint は生きているが、対象 host に利用可能な session がない。

このため、ChatGPT などの remote MCP client から接続できない場合に、Worker route、Access、host agent、session のどこを直すべきか判断しにくい。

## 目標

明示的な gateway 診断を実行すると、秘密値や physical root path を表示せず、local readiness と remote registration の状態を段階別に確認できるようにする。診断は tool call、session mutation、yolo 昇格、host token の再発行を行わない。

## 対象外

- Gateway の routing、generation/lease、MCP tool contract の変更
- Cloudflare Access policy の自動作成・自動変更
- Worker secret、Access service token、Tunnel token の自動生成または永続化
- direct Temote ingress の single-host semantics の変更
- multi-host live acceptance 自体（既存の live acceptance matrix で追跡する）

## 提案する方針

1. `temote-mcp doctor` の既存 local federation readiness を維持し、必要に応じて明示的な gateway 診断オプションまたは profile を追加する。
2. 診断結果を次の段階に分ける。
   - host ID の形式、HTTPS origin、host token の存在、Access credential の組み合わせ
   - supervisor の稼働状態、control protocol、named-root 数
   - Worker endpoint の TLS/HTTP 応答と Access 認証結果
   - host registration / lease の有無と、現在の agent generation
3. Worker 側には診断専用の read-only protocol を追加するか、既存の安全な health/status 応答を再利用する。通常の `connect` を診断目的で実行して lease や Durable Object state を変更しない。
4. route、Worker secret map、Access policy のように host から取得できない情報は、取得不能であることを明示し、誤って「ready」と判定しない。Cloudflare API credential がない場合も、local-only の結果と remote 未確認を分離する。
5. 出力は host ID、状態名、非秘密の remediation だけに限定する。token、service-token secret、Access assertion、Cookie、physical root path、raw credential-bearing environment は出力しない。

## 受け入れ条件

- [ ] gateway 診断が未設定、不完全、無効な host ID、無効な URL、supervisor unavailable を個別に報告し、非ゼロ終了する。
- [ ] host token、Access service-token、Worker secret map の値を出力せず、未設定・不一致・認証失敗を区別して報告する。
- [ ] endpoint が direct Temote か gateway Worker かを、route/response の確認結果として誤認なく示す。
- [ ] `gateway-agent` が未登録、lease expired、generation replaced、session unavailable の状態を切り分けられる。
- [ ] 診断は session、lease、tool、filesystem、Git、approval state を変更しない。
- [ ] Linux/macOS の unit/integration test で成功、設定不足、Access 失敗、remote 未確認、秘密値の非表示を固定する。
- [ ] `docs/gateway.md`、`docs/gateway.ja.md`、必要なら `README` の実行例と診断結果が同期する。

## テスト計画

- `cargo test doctor`
- `cargo test --all-targets --all-features --locked`
- gateway Worker の read-only status/health protocol test
- fake endpoint を使った TLS、HTTP、Access 認証失敗、未登録 host、lease expiry の決定論的テスト
- secret sentinel と physical root sentinel が stdout/stderr、JSON result、error path に現れないことのテスト
- 実 Cloudflare 環境では、read-only 診断と authenticated `host_list` の結果を release/commit とともに `issues/open/20260908-live-acceptance-matrix.md` に記録する

## リスク

remote status check を実装すると、Access や Worker API の一時障害を local configuration failure と誤表示する可能性がある。失敗分類を分け、診断不能を ready と扱わない必要がある。診断 endpoint を追加する場合は、host registration protocol の bearer token と MCP client の OAuth boundary を混同せず、認証なしの情報漏えいを避ける。

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Gateway federation の設定・接続状態を秘密値なしで確認できる `doctor` 診断を追加。

## 注記

- 2026-09-11: Gateway の設定要件は既存 docs に記録済みであることを確認した。今回の不足は設定仕様ではなく、Worker route、Access、host registration を横断した診断性である。
- 2026-09-11: `localmcp.obr-grp.com/*` の Worker route は direct Temote 復旧のため削除した。Gateway の再有効化は本 issue の診断・運用設計と分離する。
