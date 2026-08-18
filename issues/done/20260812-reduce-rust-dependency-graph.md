# Rust依存グラフを縮小する

Status: done
Model: GPT-5.6 Sol
Created: 2026-08-12
Updated: 2026-08-12
Branch: main
Priority: P1

## 概要

`temote-mcp` のRust依存をplatform境界と機能境界に沿って整理し、ローカルMCP利用、HTTP公開、Cloudflare gateway、sandbox実行で必要なcrateだけを対応buildへ含める。

最初にLinux専用Codex sandbox依存をtarget dependencyへ移し、その後にHTTP / Cloudflare Access / gateway機能とローカル実行機能の依存境界を整理する。

## 背景

現在の単一packageは `axum`、`jsonwebtoken`、`reqwest`、vendored `openssl`、Codex由来のsandbox関連crate、`tokio` などをまとめて直接依存している。

sandbox実装はplatform別で、Linuxでは `codex-linux-sandbox` を起動し、macOSではSeatbelt経路を利用する。
一方 `codex-linux-sandbox` は全platform共通dependencyとして宣言されている。

また `temote-mcp start` / `mcp` のローカル経路と、`serve` / `gateway-agent` のネットワーク経路が同じbinaryに入り、Cloudflare Access JWT検証用のHTTP/JWT依存も常時解決される。

## 目標

- Linux専用dependencyをLinux buildだけに限定する。
- HTTP / Cloudflare Access / gateway固有dependencyを通常のlocal-only buildから外せる構成を検討する。
- 未使用または間接的に不要なTLS / crypto dependencyを削除する。
- sandboxの安全境界、approval、Git write gate、gateway protocolを維持する。
- 変更前後の依存package数、compile time、binary sizeを記録する。

## 提案する方針

### 1. Linux専用Codex dependencyをtarget dependencyへ移す

少なくとも `codex-linux-sandbox` はLinux専用利用であるため、`[target.'cfg(target_os = "linux")'.dependencies]` へ移す。

他のCodex関連crateについても使用箇所を確認する。

- `codex-protocol`
- `codex-sandboxing`
- `codex-utils-absolute-path`

macOS Seatbelt実装でも共通型やhelperを利用しているものは共通dependencyとして残す。

### 2. vendored OpenSSLの必要性を確認する

`openssl = { features = ["vendored"] }` の直接使用箇所と推移依存上の必要性を確認する。
直接利用しておらず、`reqwest` がrustls構成で完結している場合は削除する。

削除する場合はJWT RS256検証、Cloudflare Access JWKS取得、gateway HTTPS通信のテストを必ず通す。

### 3. network機能をoptional化またはpackage分離する

次のcommand群の境界を利用する。

local:
- `start`
- `mcp`
- `doctor`

network:
- `serve`
- `gateway-agent`

`axum`、`jsonwebtoken`、`reqwest`、`url` などのnetwork依存をfeatureまたは別packageへ閉じ込められるか検討する。
公開binary名や既存commandを壊さないことを優先し、分離コストが高い場合はfeature化を先行する。

### 4. tokio featureを最小化する

現在有効な `fs`、`io-std`、`io-util`、`macros`、`net`、`process`、`rt-multi-thread`、`signal`、`sync`、`time` の各featureについて使用箇所を確認する。
不要なfeatureだけを削る。

### 5. 依存回帰を計測する

before/afterで最低限次を記録する。

- macOS buildのdependency package数
- Linux buildのdependency package数
- `cargo tree -d`
- clean release build時間
- release binary size

## 実装結果

Baseline: `0b7eff3`。計測hostはApple Silicon macOS。package数は `cargo metadata --locked --filter-platform` のresolved node数。clean buildはbefore/afterで別の空target directoryを使用した。

- `codex-linux-sandbox` をLinux target dependencyへ移動。macOS graphから除外された。`codex-protocol` / `codex-sandboxing` / `codex-utils-absolute-path` はmacOS Seatbeltでも必要なため共通dependencyを維持。
- 直接依存していた vendored `openssl` を削除。`openssl-src` もresolved graphから消えた。Cloudflare Access / JWT / JWKS / gatewayを含むall-features check/testは成功。
- `network` featureを追加し、defaultでは有効のまま既存CLI互換を維持。`--no-default-features` では `serve` / `gateway-agent` とtemote-mcp自身の `axum` / `dotenvy` / `jsonwebtoken` / `reqwest` / `url` 直接依存を除外する。Codex共通層由来のnetwork dependencyは推移依存として残る。
- `tokio` featureは全て現行コードで利用されているため削除しなかった。
- macOS Seatbelt test fixtureが書き込み許可対象の`$TMPDIR`内にdenied pathを置いていたため、sandbox policyは変更せずfixtureだけ`$HOME`配下へ移して境界テストを有効化した。
- CalVer release workflowを追加。`f4ah6o/calver-action`をcommit SHA pinし、`YYYY.MM.PATCH`（`MM`は非zero-padなので出力例は`2026.8.0`）、`Asia/Tokyo`、prefixless tag、legacy `v` prefix考慮で割り当てる。`latest`をrelease sourceとし、release-only commitで`Cargo.toml` / `Cargo.lock`を更新してからimmutable CalVer tagをpushする。

### 計測

| 項目 | before | after | 差分 |
| --- | ---: | ---: | ---: |
| macOS default package数 | 449 | 441 | -8 |
| Linux default package数 | 451 | 450 | -1 |
| `cargo tree -d` duplicate name数 | 24 | 24 | 0 |
| `cargo tree -d --depth 0` entry数 | 52 | 52 | 0 |
| clean release build時間 | 102.126 s | 85.376 s | -16.750 s (-16.4%) |
| default release binary | 9,748,432 B | 9,748,224 B | -208 B |

追加のlocal-only結果:

- macOS package数: 425
- Linux package数: 433
- macOS local-only release binary: 3,884,688 B
- local-only CLI: `doctor` / `start` / `mcp` のみ

検証:

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets --all-features` (34 passed)
- `cargo clippy --no-default-features -- -D warnings`
- `cargo test --no-default-features --locked` (24 passed)

## 受け入れ条件

- [x] macOS dependency graphにLinux sandbox helper専用crateが含まれない。
- [x] Linux sandbox実行が既存のpermission profileとLandlock経路を維持する。
- [x] macOS Seatbelt実行が維持される。
- [x] vendored OpenSSLが不要な場合は削除され、必要な場合は残す理由がIssueまたはPRに記録される。
- [x] network-only dependencyをlocal-only経路から外す方法が実装されるか、互換性上見送る場合は測定結果と理由が記録される。
- [x] Cloudflare Access JWT検証とJWKS refreshが維持される。
- [x] gateway connect / reconnect / generation分離が維持される。
- [x] approval UI、sandbox roots、Git metadata write gateを緩めない。
- [x] `cargo fmt --check` が成功する。
- [x] `cargo test` が成功する。
- [x] `cargo clippy --all-targets -- -D warnings` が成功する。
- [x] 変更前後のpackage数、compile time、binary sizeが記録される。

## 対象外

- sandbox policyの緩和
- Cloudflare gateway protocolの変更
- approvalモデルの変更
- Windows native sandboxの新規実装
- command名やMCP schemaの変更
- dependency version bumpだけを目的とする作業

## 実装順

1. platform別dependency baselineを取得する。
2. Linux専用dependencyをtarget dependencyへ移す。
3. vendored OpenSSLの必要性を検証する。
4. network機能のfeature/package境界を整理する。
5. tokio featureと重複依存を精査する。
6. sandbox / HTTP / gateway回帰テストを実行する。
7. before/afterを記録する。
