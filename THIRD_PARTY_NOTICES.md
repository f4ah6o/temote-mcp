# Third-Party Notices

This project incorporates third-party open-source software distributed under
the terms of their respective licenses.

## Upstream project

This repository is derived from
[`nakasyou/local-mcp`](https://github.com/nakasyou/local-mcp), which is
distributed under the MIT License, Copyright (c) 2026 Shotaro Nakamura. The
full text is kept in [`LICENSE`](LICENSE). Every file except the ported sources
listed below originates there or was added in this repository.

## Ported sources

- [`src/oauth.rs`](src/oauth.rs) and [`src/http.rs`](src/http.rs) are derived
  from [`f4ah6o/shuttle-rs`](https://github.com/f4ah6o/shuttle-rs)
  (`src/oauth.rs` and `src/app.rs`), dual-licensed under MIT OR Apache-2.0.

  Modifications: the SQLite-backed OAuth store was replaced by the JSON state
  file this crate already uses for sessions, refresh tokens with rotation were
  added, dynamic client registration was restricted to an allow list of
  redirect URIs, and MCP requests are dispatched through this crate's own
  stdio handler.

## Sandboxing

Linux command isolation uses `codex-sandboxing`, `codex-linux-sandbox`,
`codex-protocol`, and `codex-utils-absolute-path` from
[`openai/codex`](https://github.com/openai/codex), licensed under the Apache
License 2.0 and pinned by git revision in [`Cargo.toml`](Cargo.toml).

The macOS Seatbelt base policy in
[`src/sandbox/macos_base_policy.sbpl`](src/sandbox/macos_base_policy.sbpl) is
derived from the same OpenAI Codex revision. The surrounding local-mcp
Seatbelt policy builder is intentionally limited to local-mcp's fixed
filesystem and network contract rather than porting the general Codex
permission model. The Apache License 2.0 text is included in
[`LICENSE-APACHE`](LICENSE-APACHE). Required upstream attribution is retained:
OpenAI Codex, Copyright 2025 OpenAI.

## Rust dependencies

The authoritative list of Rust dependencies and their resolved versions is
recorded in [`Cargo.lock`](Cargo.lock). License terms, copyright notices, and
source information for each dependency are provided by the corresponding crate
package and its upstream repository.

## Distribution requirements

When redistributing this project in source or binary form, retain the license
and copyright notices required by the licenses of the bundled third-party
components.

This notice does not modify or replace the license terms of any dependency. If
this notice conflicts with a dependency's license text, the dependency's
license text governs.
