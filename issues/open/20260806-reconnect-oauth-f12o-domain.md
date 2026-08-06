# local-mcp の公開 OAuth 接続を localmcp.f12o.com で再構築する

Status: open
Model: claude-sonnet-5
Created: 2026-08-06
Updated: 2026-08-06
Branch: chore/20260806-reconnect-oauth-f12o-domain

## 進捗（2026-08-06）

- 新Tunnel `local-mcp-f12o` を作成後、`localmcp.f12o.com` の既存CNAME（`local-mcp` Tunnel、ID `b5a1469a-85d3-47e1-b0f2-dc3845578fcc`、8/2作成の作業痕跡）が見つかったため、新Tunnelは削除し、既存 `local-mcp` Tunnelへ Published application route（`localmcp.f12o.com` → `http://127.0.0.1:8791`）を設定した。
- 新規 self-hosted application `localmcp`（destination `localmcp.f12o.com`、Allow policy: `fujita.hirohito@protonmail.com`、Managed OAuth: token lifetime 15分、grant session 2週間、redirect URI 2件、localhost/loopback有効）を作成し、AUD `98e26d3569d59d06ff3a0d1c526c1e467b509eae3d268fa85f901b628976b74b` を取得。
- Team domain: `f4ah6o.cloudflareaccess.com`。
- `~/.config/local-mcp/public.env` を手動設定（モード0600に修正）、`just env-check` 成功、`just up` で `local-mcp serve` と `cloudflared tunnel run --token` を起動、3つのcurlプローブが全て期待通りの結果。
- 残りはChatGPT側での接続確認のみ。

## 概要

Cloudflare Access の self-hosted application と Tunnel を新ドメイン `localmcp.f12o.com` で新規作成し、`local-mcp serve` を新しい Managed OAuth 接続で起動し直す。

## 背景

`local-mcp` の公開 HTTP エンドポイントは Cloudflare Access が Managed OAuth を終端し、Rust 側（[src/http.rs](../../src/http.rs)）は `Cf-Access-Jwt-Assertion` を検証するだけの設計（[README.md](../../README.md) の「Public HTTP endpoint」節）。現在、`local-mcp serve` と対応する `cloudflared tunnel run` はどちらも停止しており、`~/.config/local-mcp/public.env` は空（0バイト）で `LOCAL_MCP_PUBLIC_URL` 等が未設定。運用ドメインを旧ドメインから `f12o.com` に切り替えるため、Cloudflare 側の Access application と Tunnel を作り直す。

このイシューは [20260806-purge-obr-grp-domain-history.md](../done/20260806-purge-obr-grp-domain-history.md) の完了を前提とする。README/`.env.example` が新ドメインに更新されてから着手する。

## 問題

- `local-mcp serve`（8791番）、local-mcp 用 `cloudflared tunnel run` のいずれも起動していない。
- `~/.config/local-mcp/public.env` が空で、`LOCAL_MCP_PUBLIC_URL` / `LOCAL_MCP_ACCESS_TEAM_DOMAIN` / `LOCAL_MCP_ACCESS_AUDIENCE` / `LOCAL_MCP_ACCESS_ALLOWED_EMAILS` / `LOCAL_MCP_TUNNEL_TOKEN` が未設定。
- 旧 self-hosted Access application（旧ホスト）と旧 Tunnel（名前 `local-mcp`）はドメイン移行に伴い作り直す対象であり、現行の AUD / トークンは新ドメインでは無効になる。

## 目標

- `localmcp.f12o.com` を保護する新しい self-hosted Access application が Managed OAuth 有効の状態で存在する。
- `localmcp.f12o.com` にルーティングする新しい Cloudflare Tunnel が動作している。
- `~/.config/local-mcp/public.env` に新しい値が設定され、`just env-check` が通る。
- `local-mcp serve` と対応する Tunnel が起動し、README記載の疎通確認（401 + `WWW-Authenticate`、OAuth discovery 200）が通る。
- ChatGPT 側で `localmcp.f12o.com/mcp` への新しい MCP コネクタ接続が確立している。

## 対象外

- git 履歴・working tree からの旧ドメイン除去（[20260806-purge-obr-grp-domain-history.md](../done/20260806-purge-obr-grp-domain-history.md) で扱う）。
- Cloudflare API/Bindings/Builds/Observability の MCP ツールは未認証のため、これらを用いた自動化は行わない。Cloudflare ダッシュボード（Zero Trust）での作業はユーザーが直接実施する。
- `opz`（1Password CLI ラッパー）経由の secret 注入。今回は README 記載の手動編集で `public.env` を設定する。

## 提案する方針

1. **Cloudflare Tunnel（ユーザー作業）**: 新しい Tunnel を作成し、`localmcp.f12o.com` を `http://127.0.0.1:8791` にルーティングする。Run token を控える。
2. **Cloudflare Access（ユーザー作業）**: Zero Trust > Access controls > Applications > Add application > Self-hosted で新しい self-hosted application を作成する。
   - 公開ホスト名: `localmcp.f12o.com`、パスは空（ホスト全体を保護）。
   - Allow ポリシー: `fujita.hirohito@protonmail.com` のみ。Bypass/Service Auth は追加しない。
   - Advanced 設定で Managed OAuth を有効化: dynamic client registration 有効、アクセストークン寿命15分、grant session 期間14日。
   - Redirect URI: `https://chatgpt.com/connector/oauth/*` と `https://chatgpt.com/connector_platform_oauth_redirect`。localhost/loopback client オプションも有効化する（README 記載の既知動作構成を踏襲）。
   - 新しい AUD を控える。
3. **ローカル設定（ユーザーが値を入力、値の適用はコマンド実行で確認）**: `~/.config/local-mcp/public.env` を `vi` 等で編集し、`LOCAL_MCP_PUBLIC_URL=https://localmcp.f12o.com`、`LOCAL_MCP_ACCESS_TEAM_DOMAIN`、新しい `LOCAL_MCP_ACCESS_AUDIENCE`、`LOCAL_MCP_ACCESS_ALLOWED_EMAILS=fujita.hirohito@protonmail.com`、新しい `LOCAL_MCP_TUNNEL_TOKEN` を設定する。
4. `just env-check` で必須変数が揃っていることを確認する。
5. `just up`（または `just serve` / `just tunnel` を別ターミナルで）で起動する。
6. README記載の curl プローブで疎通を確認する。
7. ChatGPT の Developer mode で `https://localmcp.f12o.com/mcp` への新しい MCP コネクタ接続を追加する。

## 受け入れ条件

- [x] `curl -i -X POST https://localmcp.f12o.com/mcp ...`（未認証）が `401` と `WWW-Authenticate` ヘッダーを返す。
- [x] `curl -i https://localmcp.f12o.com/.well-known/oauth-authorization-server` が `200` を返す。
- [x] `curl -i https://localmcp.f12o.com/.well-known/oauth-protected-resource` が `200` を返す。
- [x] `just env-check` が成功する。
- [ ] `local-mcp serve` のログにエラーがなく、認証成功後の `tools/list` が期待する12ツールを返す。
- [ ] ChatGPT から `localmcp.f12o.com/mcp` への接続でツール一覧が表示される。

## テスト計画

- `just env-check`
- `just up`（または `just serve` と `just tunnel` を別ターミナルで）
- README「Before connecting」節の3つの curl コマンド
- ChatGPT Developer mode でのコネクタ追加と `tools/list` 確認

## リスク

- Cloudflare 側の作業（Tunnel/Access application 作成）は本セッションの Cloudflare MCP ツールが未認証のため代行できず、手順どおりに手動で行われる前提。設定漏れ（AUD の取り違え、パス制限の誤設定）が起きると README の既知のトラブルシュート節（`Cloudflare Access JWT audience is invalid` 等）に該当する。
- `public.env` はモード 0600 を維持する。誤って平文のままコミットしないよう `.gitignore` 対象であることを確認する。
- 新旧 Tunnel/Access application が並行して存在する期間は、どちらの設定が有効か混乱しやすい。旧設定の無効化タイミングを明確にする。

## 変更履歴

`CHANGES.md` impact: no

## 注記

- [20260806-purge-obr-grp-domain-history.md](../done/20260806-purge-obr-grp-domain-history.md) の完了後に着手する。
