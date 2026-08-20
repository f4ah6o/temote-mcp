# Temote MCP

[English](README.md)

Temote MCP は、手元のファイル・コマンド・一部のホスト連携を MCP ツールとして公開し、通常セッションではサンドボックスとローカル承認を維持する MCP サーバーです。

## インストール

ビルド済みバイナリは `cargo-binstall` で導入できます。

```sh
cargo binstall --git https://github.com/f4ah6o/temote-mcp temote-mcp
temote-mcp doctor
```

ソースからビルドする場合は次を使います。

```sh
cargo install --git https://github.com/f4ah6o/temote-mcp --locked
```

Apple Silicon Mac と Linux に対応しています。Intel Mac と Windows ネイティブは未対応で、gateway endpoint では WSL2 を利用できます。

## セッションを開始する

常駐 host では named root を設定して HTTP supervisor を起動します。

```sh
# host の例:
# ~/src -> /Volumes/devstorage/Developer
export TEMOTE_MCP_ROOTS='src=~/src'
just up
```

認証済み MCP client からは次の順で利用します。

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
```

managed session は常に通常の sandbox session です。`session_start` は named root からの相対 path のみ受け付け、yolo mode は指定できません。host/network 操作の承認は `just up` を実行しているローカル端末で行います。

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
- [公開 HTTP endpoint と Cloudflare Access](docs/public-http.ja.md)
- [1Password / kintone 連携](docs/integrations.ja.md)
- [multi-host Cloudflare gateway](docs/gateway.ja.md)
- [build / test / release](docs/development.md)

このリポジトリを編集する coding agent 向けの指示は [AGENTS.md](AGENTS.md) にあります。

## 由来とライセンス

このプロジェクトは [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) から派生しています。**Temote** の名前は、[@mr_konn が「remote」の対義語として提唱した「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46) に着想を得ています。詳細な attribution は [THIRD_PARTY_NOTICES.ja.md](THIRD_PARTY_NOTICES.ja.md) を参照してください。

ライセンスはリポジトリ内の MIT / Apache-2.0 ライセンスファイルに従います。
