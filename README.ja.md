# Temote MCP

[English](README.md)

Temote MCP は、手元のファイル・コマンド・一部のホスト連携を MCP ツールとして公開し、通常セッションではサンドボックスとローカル承認を維持する MCP サーバーです。

## インストール

ビルド済みバイナリは `cargo-binstall` で導入できます。

```sh
cargo binstall temote-mcp
temote-mcp doctor
```

ソースからビルドする場合は次を使います。

```sh
cargo install temote-mcp --locked
```

旧 `just up` 構成を置き換える場合は、先に binary を更新し、migration を確認・適用してから現在の supervisor を起動します。

```sh
cargo binstall temote-mcp --force
temote-mcp migrate --dry-run
temote-mcp migrate
temote-mcp up --profile cloudflare
```

`migrate` は旧 runtime ownership に加えて、互換な旧 Cloudflare 設定も移行します。既存の `public.env` や内容の異なる既存 Tunnel token は上書きせず、checkout-local `.env` からは Temote が必要とする Cloudflare/runtime key だけをコピーします。別途 `temote-mcp start` した local session は停止しません。`temote-mcp up --profile cloudflare` も、移行先がまだ無い場合は同じ互換設定 migration を bootstrap できます。

macOS の canonical default は `~/.config/temote-mcp/public.env` です。以前の実装で誤って使われる可能性があった `~/Library/Application Support/temote-mcp/public.env` は migration source として認識します。Linux は通常の config directory semantics を維持し、`TEMOTE_MCP_ENV_FILE` を明示した場合はその path を優先します。

Apple Silicon Mac と Linux に対応しています。Intel Mac と Windows ネイティブは未対応で、gateway endpoint では WSL2 を利用できます。

## セッションを開始する

常駐 host では named root を設定して HTTP supervisor を起動します。

```sh
# host の例:
# ~/src -> /Volumes/devstorage/Developer
export TEMOTE_MCP_ROOTS='src=~/src'
# 既存構成は Cloudflare profile が default です。
temote-mcp up --profile cloudflare
# Tailscale Funnel + Temote local OAuth を使う場合:
# temote-mcp up --profile tailscale
# outbound-only の OpenAI Secure MCP Tunnel を bootstrap / 利用する場合。
# 対応する環境変数が無ければ、どちらも terminal echo 無効の hidden prompt で key を入力します:
# temote-mcp openai setup --workspace-id <workspace-id>
# temote-mcp up --profile openai
```

認証済み MCP client からは次の順で利用します。

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
```

managed session は常に通常の sandbox session です。`session_start` は named root からの相対 path のみ受け付け、yolo mode は指定できません。host/network 操作の承認は `temote-mcp up` を実行しているローカル端末で行います。停止は `temote-mcp down` です。

従来どおり、ローカル session を直接起動することもできます。

```sh
cd ~/src/my-project
temote-mcp start my-project
```

local stdio client は `temote-mcp mcp` を起動します。制限を意図的に外す CLI session は `temote-mcp start my-project --yolo` で利用できます。

## Agent Skill

Temote MCP には、session、Git 専用ツール、background job、bridge MCP を AI が適切に使うための Agent Skill が含まれています。

```sh
gh skill install f4ah6o/temote-mcp temote-mcp --scope user
```

必要に応じて `--agent codex`、`--agent claude-code` など対象 agent を指定します。

## 詳細ドキュメント

- [session と tool の使い方](docs/usage.ja.md)
- [managed session と named root](docs/managed-sessions.ja.md)
- [remote connection profile: Cloudflare / Tailscale / OpenAI Secure MCP Tunnel](docs/public-http.ja.md)
- [1Password / kintone 連携](docs/integrations.ja.md)
- [multi-host Cloudflare gateway](docs/gateway.ja.md)
- [Linux sandbox と crates.io packaging](docs/linux-sandbox.ja.md)
- [build / test / release](docs/development.md)

repository checkout では `just up` / `just down` が checkout の binary を build・選択して CLI へ委譲する開発用 wrapper です。インストール済み binary の運用に `just` は必要ありません。

このリポジトリを編集する coding agent 向けの指示は [AGENTS.md](AGENTS.md) にあります。

## 由来とライセンス

このプロジェクトは [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) から派生しています。**Temote** の名前は、[@mr_konn が「remote」の対義語として提唱した「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46) に着想を得ています。詳細な attribution は [THIRD_PARTY_NOTICES.ja.md](THIRD_PARTY_NOTICES.ja.md) を参照してください。

ライセンスはリポジトリ内の MIT / Apache-2.0 ライセンスファイルに従います。
