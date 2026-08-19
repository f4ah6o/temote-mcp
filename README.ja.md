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

AI に触らせたいプロジェクトのディレクトリで起動します。

```sh
cd ~/src/my-project
temote-mcp start my-project
```

MCP client からローカル stdio server を起動するコマンドは次です。

```sh
temote-mcp mcp
```

`session_list` 以外のツール呼び出しでは session ID を指定します。セッションを起動した端末で、承認と許可ディレクトリを管理できます。

```text
/permission allow ../another-project
/permission revoke ../another-project
/permission list
```

意図的に制限を外す場合だけ `--yolo` を使います。

```sh
temote-mcp start my-project --yolo
```

`--yolo` では Temote MCP のパス制限、サンドボックス、ローカル承認が無効になります。

## Agent Skill

Temote MCP には、session、Git 専用ツール、background job、bridge MCP を AI が適切に使うための Agent Skill が含まれています。

```sh
gh skill install f4ah6o/temote-mcp temote-mcp --scope user
```

必要に応じて `--agent codex`、`--agent claude-code` など対象 agent を指定します。

## 詳細ドキュメント

- [session と tool の使い方](docs/usage.ja.md)
- [公開 HTTP endpoint と Cloudflare Access](docs/public-http.ja.md)
- [1Password / kintone 連携](docs/integrations.ja.md)
- [multi-host Cloudflare gateway](docs/gateway.ja.md)
- [build / test / release](docs/development.md)

このリポジトリを編集する coding agent 向けの指示は [AGENTS.md](AGENTS.md) にあります。

## 由来とライセンス

このプロジェクトは [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) から派生しています。**Temote** の名前は、[@mr_konn が「remote」の対義語として提唱した「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46) に着想を得ています。詳細な attribution は [THIRD_PARTY_NOTICES.ja.md](THIRD_PARTY_NOTICES.ja.md) を参照してください。

ライセンスはリポジトリ内の MIT / Apache-2.0 ライセンスファイルに従います。
