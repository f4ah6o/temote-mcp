# release CI の floating stable Clippy と local gate の lint set がずれて release 時だけ失敗する

Status: open
Model: GPT-5.6 Sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 release reliability
Type: CI / Rust toolchain / release gate

## Observed

`latest` tag から起動した Allocate Release run `35062827543` は source `5115ed574084e62588e3fa99f422403ce2f68c9a` を `2026.9.10` に書き換えた後、`Validate release` で失敗した。

GitHub Actions runner の Clippy 1.98.0 では次の2箇所が新 lint `clippy::chunks_exact_to_as_chunks` により `-D warnings` で failure になった。

- `src/supervisor.rs:1719`
- `src/supervisor.rs:1796`

release workflow は `dtolnay/rust-toolchain@stable` を使用する一方、repository には `rust-toolchain.toml` / `rust-toolchain` / Cargo `rust-version` がなく、local developer gate で使う toolchain と release CI の lint set が一致しない。

今回 local `cargo clippy --all-targets --all-features -- -D warnings` は PASS していたが、release CI の newer Clippy だけが failure になった。

## Impact

- repository-local deterministic gate が green でも release workflow が新しい Clippy lint で突然失敗する。
- CalVer allocation 後、immutable tag / crates.io publish / cargo-dist dispatch の前で止まる。
- release trigger の再実行と原因調査が必要になり、approve-free developer workflow の摩擦になる。

## Proposed direction

次のいずれかで local/CI toolchain contract を明示する。

1. `rust-toolchain.toml` で release CI と local development の channel/version/components を統一する。
2. あるいは CI 側の exact toolchain version を repository-managed config から解決する。
3. toolchain update は Dependabot/Renovate相当または dedicated PR で意図的に行い、そのPRで新 lint を修正する。

`stable` の暗黙追随は release job だけで初めて新 lint を受けるため避ける。

## Acceptance criteria

- [ ] local `cargo clippy --all-targets --all-features -- -D warnings` と release CI が同じ Rust/Clippy toolchainを使う。
- [ ] toolchain version/channel が repository-local source of truth で確認できる。
- [ ] toolchain更新時に通常CIで新 lint failureを検出し、release trigger時まで持ち越さない。
- [ ] release workflow の `Set up Rust` が repository-managed toolchain contract と一致する。
- [ ] `2026.9.10` release failure の再発防止根拠を記録する。
