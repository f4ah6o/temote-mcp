# Third-Party Notices 日本語版

[English](THIRD_PARTY_NOTICES.md)

> この文書は参照用の日本語版です。ライセンス条件そのものは各原文ライセンスおよび英語の notice を優先します。

本プロジェクトは、それぞれのライセンス条件に従って配布される第三者のオープンソースソフトウェアを含みます。

## Upstream project

本リポジトリは [`nakasyou/local-mcp`](https://github.com/nakasyou/local-mcp) から派生しています。upstream は MIT License、Copyright (c) 2026 Shotaro Nakamura の条件で配布されています。ライセンス全文は [`LICENSE`](LICENSE) に保持しています。下記の ported source を除き、各 file は upstream に由来するか、本リポジトリで追加されたものです。

## Ported sources

- [`src/http.rs`](src/http.rs) は [`f4ah6o/shuttle-rs`](https://github.com/f4ah6o/shuttle-rs) の `src/app.rs` を起点としており、MIT OR Apache-2.0 の dual license です。過去の revision では OAuth 実装も port していましたが、Managed OAuth の境界を Cloudflare Access に移した際に削除しました。現在の HTTP layer は MCP request を local-mcp 独自 handler に dispatch し、origin で Cloudflare Access を検証します。

## Sandboxing

Linux の command isolation は [`openai/codex`](https://github.com/openai/codex) の `codex-sandboxing`、`codex-linux-sandbox`、`codex-protocol`、`codex-utils-absolute-path` を使用します。これらは Apache License 2.0 で、本リポジトリの [`Cargo.toml`](Cargo.toml) では Git revision に固定しています。

macOS Seatbelt base policy [`src/sandbox/macos_base_policy.sbpl`](src/sandbox/macos_base_policy.sbpl) も同じ OpenAI Codex revision から派生しています。周辺の local-mcp Seatbelt policy builder は、Codex の汎用 permission model 全体を port するのではなく、local-mcp 固有の filesystem/network contract に限定しています。Apache License 2.0 の全文は [`LICENSE-APACHE`](LICENSE-APACHE) に含まれます。必要な upstream attribution として OpenAI Codex, Copyright 2025 OpenAI を保持します。

## Rust dependencies

Rust dependency と解決済み version の authoritative list は [`Cargo.lock`](Cargo.lock) に記録されています。各 dependency のライセンス条件、copyright notice、source 情報は対応する crate package と upstream repository に従います。

## Distribution requirements

本プロジェクトを source または binary で再配布する場合は、同梱する第三者 component のライセンスが要求する license/copyright notice を保持してください。

この notice は各 dependency のライセンス条件を変更・置換するものではありません。この文書と dependency のライセンス原文が矛盾する場合は、ライセンス原文を優先します。
