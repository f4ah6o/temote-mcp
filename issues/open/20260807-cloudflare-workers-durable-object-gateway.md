# Cloudflare Workers + Durable Objects によるサーバレス gateway を実装する

Status: open
Model: gpt-5.6-thinking
Created: 2026-08-07
Updated: 2026-08-07
Branch: main

## 進捗（2026-08-07）

- Worker / Durable Objects / endpoint agent / documentation / tests の実装は完了。
- `cargo test` 32件、`cargo clippy --all-targets -- -D warnings`、Worker test 8件、Node syntax check が成功。
- Wrangler はローカル未導入で、現在の sandbox はネットワーク無効のため deploy dry-run は未実施。
- 現在接続中の Local MCP は Git metadata 書込み用 tool を公開しておらず、sandboxed `git switch` / `git add` は `.git` への書込みを拒否するため、commit / merge / push は未完了。

## 概要

ChatGPT には単一の MCP endpoint だけを公開し、Cloudflare Workers と Durable Objects が `session_id` ごとに Mac / Windows 側 host agent へ要求をルーティングする。各端末の agent は inbound port を公開せず、gateway へ outbound HTTP long-poll 接続する。

## 背景

現行構成は `local-mcp serve` と端末ごとの Cloudflare Tunnel を直接結び、公開 endpoint と host が一対一になっている。複数端末・複数 session を扱うには ChatGPT 側の MCP connection が増え、端末の切断・再接続や stale response の扱いも origin ごとに分散する。

## 目標

- ChatGPT からは単一 MCP として見える。
- Mac と Windows は別 `session_id` を使う。
- 各端末の agent は gateway へ outbound 接続する。
- gateway は Cloudflare Workers + Durable Objects で構成する。
- `session_id` ごとに対応 host へルーティングする。
- gateway 接続開始時に端末側 terminal で承認する。
- 切断、再接続、世代交代を扱い、旧 generation の応答を受理しない。
- Windows native 対応前は WSL2 上の agent を正式な暫定経路として扱う。

## アーキテクチャ

```text
ChatGPT
  |
  | Managed OAuth / MCP Streamable HTTP
  v
Cloudflare Worker (/mcp)
  |
  +--> Registry Durable Object
  |      active session lease一覧
  |
  +--> Session Durable Object (id = session_id)
         request queue / generation / pending response
            ^
            | outbound connect + long poll
            |
       local-mcp gateway-agent
            |
            +--> local session socket
                  approval UI / sandbox roots / command execution
```

## プロトコル

1. agent が `/v1/hosts/connect` に `session_id`, `instance_id`, `platform` を送る。
2. Session Durable Object が generation を単調増加させ、旧 host・旧 pending request を失効させる。
3. agent は `/v1/hosts/poll` を long-poll し、gateway request を取得する。
4. Worker の `/mcp` は `tools/call` 内の `session_id` から Durable Object を選択する。
5. agent は既存 `mcp::dispatch_public` を使って端末側で処理し、`/v1/hosts/respond` へ返す。
6. response の `generation` と `instance_id` が現行 host と一致しない場合、Durable Object は `409 stale_generation` として破棄する。
7. lease timeout、明示 disconnect、再 connect 時に pending request を失敗させる。

## セキュリティ境界

- ChatGPT 側は Cloudflare Access Managed OAuth の `Cf-Access-Jwt-Assertion` を Worker で検証する。
- host API は Worker secret `HOST_TOKEN` を bearer token として要求する。
- Cloudflare Access で host route も保護する場合、agent は service token header を付与できる。
- `gateway-agent` 起動時に local session terminal へ `gateway_connect` 承認を要求する。
- 実処理は既存 sandbox root、symlink check、job limit、Git approval gate を継承する。
- public endpoint から `without_sandbox` は公開しない。

## 実装

- [x] `gateway/` に Worker、Session Durable Object、Registry Durable Object を追加する。
- [x] 単一 `/mcp` endpoint で initialize / tools/list / tools/call を提供する。
- [x] `session_id` による Durable Object routing を追加する。
- [x] active session lease を Registry Durable Object で管理する。
- [x] connect / poll / respond / disconnect host protocol を追加する。
- [x] generation 分離と stale response 拒否を追加する。
- [x] `local-mcp gateway-agent` を追加する。
- [x] gateway 接続前の terminal approval を追加する。
- [x] Mac / Linux / WSL2 platform reporting を追加する。
- [x] Cloudflare Access service token header を agent option として追加する。
- [x] README に deploy・運用・WSL2 暫定手順を追加する。
- [x] Rust unit test と Worker protocol unit test を追加する。

## 受け入れ条件

- [x] Worker の `tools/list` が単一 MCP の tool schema を返す。
- [x] `session_list` が Registry Durable Object の active lease を返す。
- [x] `tools/call` が `params.arguments.session_id` の Durable Object にだけ配送される。
- [x] host 再接続ごとに generation が増える。
- [x] 旧 generation / instance からの response は拒否される。
- [x] host offline、request timeout、host replacement が MCP error になる。
- [x] agent は endpoint terminal の承認なしに gateway 接続しない。
- [x] Mac と WSL2 で同じ agent command を利用できる。
- [ ] 実 Cloudflare account に Durable Object exports configuration と secrets を適用する。
- [ ] ChatGPT Managed OAuth 接続から Mac / WSL2 の2 session を同時に live 検証する。
- [ ] 実装差分を commit して main へ反映する。

## テスト計画

- `cargo fmt --check`
- `cargo test`
- `cd gateway && npm test`
- `cd gateway && npx wrangler deploy --dry-run`（wrangler取得可能な環境）
- local Worker emulator と2つの `gateway-agent` を使った connect / reconnect / stale generation 検証
- Cloudflare Access Managed OAuth 経由の ChatGPT `initialize`, `tools/list`, `session_list`, `tools/call`

## Windows 方針

現行 local session transport と sandbox は Unix API を使用するため、Windows native は対象外とする。Windows端末では WSL2 内で `local-mcp start <windows-session-id>` と `local-mcp gateway-agent` を動かす。native Windows transport と sandbox backend が実装された時点で platform を `windows` に切り替えるが、gateway protocol と generation model は変更しない。

## リスク

- Durable Object の active request は in-memory waiter を使うため、実行中 request の Worker upgrade は retryable error になる。request は非冪等操作を含むため gateway は自動再送しない。
- `session_id` は routing key であり credential ではない。host token、Access JWT、terminal approval の代替にしない。
- Registry lease は表示用であり、最終的な online 判定は Session Durable Object が行う。
- agent の long-poll timeout と Durable Object request timeout は Cloudflare の実行制限より短く保つ。

## 変更履歴

`CHANGES.md` impact: no
