# Temote MCP

[English](README.md)

Temote MCP は、明示的な sandbox session を通じて、ローカルマシンでの作業を coding agent に委譲します。Codex app-server、OpenCode serve、Devin ACP、Devin Cloud は、保持された task、型付き control、上限付き evidence を共通に利用します。Temote Fabric は、複数の Host を 1 つの認証済み MCP endpoint へ接続します。

## インストール

現在のソースを Rust でビルドします。

```sh
cargo install --path . --locked
temote doctor
```

[ビルド済みリリース](https://github.com/f4ah6o/temote-mcp/releases) は `cargo-binstall` でも導入できます。次の CalVer リリースまでは既存の `temote-mcp` package 名が維持され、その後は `cargo binstall temote` で導入できます。特定の版に固定する場合は `cargo binstall temote@<version>` を使います。このソースの正規 package / command 名は `temote` で、`temote-mcp` executable は互換 alias として残ります。[移行ガイド](docs/naming-migration.md)を参照してください。

## 最初の session

supervisor の起動環境に named root を設定し、その配下で起動します。

```sh
export TEMOTE_ROOTS='src=~/src'
cd ~/src/your-project
temote start work
```

新しい session は `agent` permission mode で、設定済み named root が必要です。root は supervisor の起動前に設定してください。起動済み supervisor は既存の設定を保持します。委譲した task は session の canonical scope に制限されます。ローカル境界を意図的に解除する場合にのみ `--yolo` を指定してください。

同梱の Codex plugin を導入します。

```sh
temote codex plugin install
```

他の Agent Skill 対応 client では、次を実行します。

```sh
gh skill install f4ah6o/temote-mcp temote-mcp --scope user
```

session を選び、backend を確認し、新しい `operation_id` で task を開始します。保持された状態と scoped evidence で結果を確認してください。詳細は[使い方](docs/usage.ja.md)にあります。

## ドキュメント

- [session と task tool](docs/usage.ja.md)
- [managed session と named root](docs/managed-sessions.ja.md)
- [MCP 認証と ingress](docs/public-http.ja.md)
- [Temote Fabric](docs/gateway.ja.md)
- [build / test / release](docs/development.md)
- [repository の agent 向け指示](AGENTS.md)

## 由来とライセンス

このプロジェクトは [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) から派生しています。**Temote** の名前は、[@mr_konn が remote の対義語として提唱した「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46) に着想を得ています。[attribution](THIRD_PARTY_NOTICES.ja.md) を参照してください。ライセンスは MIT / Apache-2.0 です。
