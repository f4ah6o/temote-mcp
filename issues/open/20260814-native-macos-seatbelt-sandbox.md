# macOS sandbox を Codex 依存から分離して native Seatbelt backend 化する

Status: open
Model: GPT-5.6 Sol
Created: 2026-08-14
Updated: 2026-08-15
Branch: main
Priority: P1

## 概要

macOS の sandbox 実行から `codex-protocol` / `codex-sandboxing` / `codex-utils-absolute-path` を除去し、local-mcp が実際に必要とする固定権限モデルと Seatbelt profile 生成だけを local-mcp 側で所有する。Linux は既存 Codex sandbox stack を維持する。

Apple Silicon macOS の resolved graph は 441 -> 193 packages、248 packages / 56.2% 削減。x86_64 macOS は 442 -> 194 packages。macOS graph の `codex-*` は 0 になった。

## 実装結果

- `SandboxSpec` を導入し、macOS の filesystem / network / Git 例外権限を local-mcp 固有の小さい policy model に集約した。
- macOS production path を `/usr/bin/sandbox-exec` を直接使う native Seatbelt backend に切り替えた。
- Seatbelt dynamic path は `-D` parameter 経由に限定し、非UTF-8 path は fail-closed とした。
- network allow rule を持たない default-deny policy を維持した。
- cwd / permitted roots / `/tmp` / `$TMPDIR` の write を許可し、それ以外の filesystem write を拒否する。
- `.git` / `.agents` / `.codex` を通常 command から保護する。
- `git_add` / `git_commit` のときだけ Git metadata の必要部分を writable にし、`config`, `hooks`, `refs/tags`, `refs/remotes`, `objects/pack` 等は read-only のまま維持する。
- broad permitted root が nested workspace の `.git` 保護を迂回しないよう、親 writable rule から nested protected metadata を除外する。
- `codex-protocol` / `codex-sandboxing` / `codex-utils-absolute-path` を Linux-only dependency へ移動した。
- Codex dependency の feature unification に偶然依存していた `uuid/serde` を直接宣言した。
- MCP tool description を platform-neutral な `local-mcp sandbox` 表現へ更新した。
- `doctor` に `native macOS Seatbelt` backend 表示を追加し、macOS 26 で実在する `/usr/bin/true` を sandbox probe に使用するよう修正した。
- package license metadata を `MIT AND Apache-2.0` に整理し、`LICENSE-APACHE` と OpenAI Codex attribution を同梱した。
- Linux/macOS matrix CI を追加し、macOS graph の Codex=0 / package上限と Linux Codex stack / package上限を固定した。

## Security contract

macOS 実hostで以下を runtime test 済み。

Filesystem:

- [x] workspace create/write 成功。
- [x] extra writable root write 成功。
- [x] workspace 外 create/update/delete 失敗。
- [x] `/tmp` / `$TMPDIR` 利用成功。
- [x] symlink escape 失敗。
- [x] rename escape 失敗。
- [x] hardlink escape 失敗。
- [x] broad writable parent から nested `.git` への迂回失敗。

Network:

- [x] sandboxed outbound network access 失敗。

Git:

- [x] normal command から `.git/index` write 失敗。
- [x] native Git mode で実 `git add` 成功。
- [x] native Git mode で実 `git commit` 成功。
- [x] `.git/config` write 失敗。
- [x] `.git/hooks` write 失敗。
- [x] `refs/tags` write 失敗。
- [x] `refs/remotes` write 失敗。
- [x] `objects/pack` write 失敗。
- [x] linked worktree で実 `git add` / `git commit` 成功。
- [x] unrelated `.git` pointer escalation 拒否。

## MCP live acceptance

新しい release binary を通常session（`yolo=false`）で起動し、MCP JSON-RPC 経路から以下を連続実行した。

- [x] `write_file`
- [x] `execute`
- [x] `start_command` + `poll_job`
- [x] `git_add`
- [x] `git_commit`
- [x] commit 後 working tree clean
- [x] `doctor`: 0 failure / 0 warning

live acceptance result: `MCP_LIVE_ACCEPTANCE=PASS`。

## Dependency acceptance

| target | before | after | Codex after |
| --- | ---: | ---: | ---: |
| aarch64-apple-darwin | 441 | 193 | 0 |
| x86_64-apple-darwin | 442 | 194 | 0 |
| x86_64-unknown-linux-gnu | 450 | 450 | 14 packages / stack維持 |

- [x] aarch64 macOS = 193 packages。
- [x] x86_64 macOS = 194 packages。
- [x] macOS resolved graph の `codex-*` = 0。
- [x] Linux resolved graph = 450 packages。
- [x] Linux Codex Git metadata policy unit test を維持し、CIで実行する。

## Controlled measurement

Baseline SHA: `724bf834ebca4b568214ac6709f2159ed3471652`。
Apple Silicon macOS。同一hostで before/after を別の空 `CARGO_TARGET_DIR` に置き、`CARGO_INCREMENTAL=0`、`cargo build --release --locked` で計測した。root rebuild は clean build 後に `src/main.rs` の mtime を更新して再buildした。

| metric | before | after | change |
| --- | ---: | ---: | ---: |
| resolved packages | 441 | 193 | -248 (-56.2%) |
| duplicate package names | 22 | 4 | -18 (-81.8%) |
| clean release build | 179.176 s | 35.332 s | -143.844 s (-80.3%) |
| root-crate rebuild | 35.630 s | 10.894 s | -24.736 s (-69.4%) |
| release binary | 9,763,664 B | 7,090,912 B | -2,672,752 B (-27.4%) |
| Cargo.lock packages | 520 | 520 | unchanged; Linux Codex stackを保持 |

## Local final gates

- [x] `cargo fmt --check`
- [x] `cargo clippy --all-targets --all-features -- -D warnings`
- [x] `cargo test --all-targets --all-features --locked` — 47 passed
- [x] `cargo test --no-default-features --locked` — 37 passed
- [x] `cargo build --release --locked`
- [x] `cargo build --release --locked --no-default-features`
- [x] aarch64/x86_64 macOS + x86_64 Linux metadata boundary check
- [x] `local-mcp doctor` — native macOS Seatbelt / 0 failure / 0 warning
- [x] `git diff --check`

## Commit separation

security behavior change と dependency graph change は分離した。

1. `eca51e1` `refactor: use native Seatbelt sandbox on macOS`
2. `d79bbfc` `build: scope Codex sandbox dependencies to Linux`

## 残り

- [ ] 上記commitを `main` へpushする。
- [ ] GitHub Actions `CI` の Ubuntu / macOS jobs が green になることを確認する。
- [ ] green確認後、このIssueを `issues/done` へ移動する。

## 変更履歴

`CHANGES.md` impact: no
