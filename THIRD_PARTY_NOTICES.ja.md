# Third-Party Notices 日本語版

[English](THIRD_PARTY_NOTICES.md)

> これは参照用の日本語訳です。ライセンス条件は各ライセンスの原文に従います。

このプロジェクトには、各ライセンスの条件に従って配布される第三者のオープンソースソフトウェアが含まれます。

## Upstream project

このリポジトリは [`nakasyou/local-mcp`](https://github.com/nakasyou/local-mcp) から派生しています。upstream は MIT License、Copyright (c) 2026 Shotaro Nakamura の条件で配布されています。ライセンス全文は [`LICENSE`](LICENSE) にあります。下記の移植コードを除き、各ファイルは upstream に由来するか、このリポジトリで追加されたものです。

## Ported sources

[`src/http.rs`](src/http.rs) は [`f4ah6o/shuttle-rs`](https://github.com/f4ah6o/shuttle-rs) の `src/app.rs` をもとにしており、MIT OR Apache-2.0 のデュアルライセンスです。以前の revision では OAuth 実装も移植していましたが、Managed OAuth の境界を Cloudflare Access へ移した際に削除しました。現在の HTTP layer は MCP request を temote-mcp の handler へ渡し、origin で Cloudflare Access を検証します。

## Sandboxing

Linux sandbox 実装 [`src/sandbox/linux/`](src/sandbox/linux/) は、Apache License 2.0 で提供される [`openai/codex`](https://github.com/openai/codex) revision `20fedafff83f5c681fc62f73b0ca3227e42e3f8b` を調査して開発しています。取り込んだ範囲は Codex の sandboxing / linux-sandbox component にある Linux sandbox の概念と挙動に限定し、filesystem isolation、bubblewrap namespace、restricted network/seccomp hardening、helper execution を Temote 用に縮約しています。Codex の permission/protocol topology は持ち込まず、Temote 固有の最小 policy と、同一 `temote-mcp` Cargo package に含まれる `temote-linux-sandbox` helper に置き換えています。参照した upstream 箇所、意図的に除外した component、local modification は [`docs/linux-sandbox.md`](docs/linux-sandbox.md) に記録しています。

macOS 用の Seatbelt base policy [`src/sandbox/macos_base_policy.sbpl`](src/sandbox/macos_base_policy.sbpl) も、同じ OpenAI Codex revision から派生しています。temote-mcp 側の Seatbelt policy builder は Codex の汎用 permission model 全体を移植せず、temote-mcp が必要とする filesystem/network の制約だけを実装しています。Apache License 2.0 の全文は [`LICENSE-APACHE`](LICENSE-APACHE) に収録しています。両 sandbox 実装に必要な attribution として OpenAI Codex, Copyright 2025 OpenAI を保持します。

## Rust dependencies

Rust の依存関係と解決済み version は [`Cargo.lock`](Cargo.lock) が正本です。各 dependency のライセンス条件、copyright notice、source 情報は、それぞれの crate package と upstream repository に従います。

## Distribution requirements

このプロジェクトを source または binary で再配布する場合は、同梱する第三者 component のライセンスが求める license/copyright notice を残してください。

この notice は、各 dependency のライセンス条件を変更したり置き換えたりするものではありません。内容がライセンス原文と矛盾する場合は、ライセンス原文が優先されます。
