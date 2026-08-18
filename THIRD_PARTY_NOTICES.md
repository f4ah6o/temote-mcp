# Third-Party Notices

[日本語](THIRD_PARTY_NOTICES.ja.md)

This project includes third-party open-source software under its respective licenses.

## Upstream project

This repository is derived from [`nakasyou/local-mcp`](https://github.com/nakasyou/local-mcp), distributed under the MIT License, Copyright (c) 2026 Shotaro Nakamura. The full license text is in [`LICENSE`](LICENSE). Except for the ported source noted below, files either originate from that upstream project or were added in this repository.

## Ported sources

[`src/http.rs`](src/http.rs) originated from [`f4ah6o/shuttle-rs`](https://github.com/f4ah6o/shuttle-rs) (`src/app.rs`), dual-licensed under MIT OR Apache-2.0. Earlier revisions also ported its OAuth implementation. That code was removed when Cloudflare Access became the Managed OAuth boundary. The current HTTP layer dispatches MCP requests through temote-mcp's own handler and validates Cloudflare Access at the origin.

## Sandboxing

Linux command isolation uses `codex-sandboxing`, `codex-linux-sandbox`, `codex-protocol`, and `codex-utils-absolute-path` from [`openai/codex`](https://github.com/openai/codex). They are licensed under the Apache License 2.0 and pinned to a Git revision in [`Cargo.toml`](Cargo.toml).

The macOS Seatbelt base policy in [`src/sandbox/macos_base_policy.sbpl`](src/sandbox/macos_base_policy.sbpl) is derived from the same OpenAI Codex revision. temote-mcp's Seatbelt policy builder does not port the general Codex permission model; it implements the filesystem and network boundaries needed by temote-mcp. The Apache License 2.0 text is included in [`LICENSE-APACHE`](LICENSE-APACHE). Required upstream attribution is retained: OpenAI Codex, Copyright 2025 OpenAI.

## Rust dependencies

[`Cargo.lock`](Cargo.lock) is the authoritative list of resolved Rust dependencies and versions. License terms, copyright notices, and source information for each dependency come from the corresponding crate package and upstream repository.

## Distribution requirements

When redistributing this project in source or binary form, retain the license and copyright notices required by the bundled third-party components.

This notice does not change or replace any dependency's license terms. If it conflicts with a dependency's license text, the license text governs.
