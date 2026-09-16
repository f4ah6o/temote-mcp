# agent mode で標準 language build cache が read-only になり通常の test が失敗する

Status: doing / private build-cache first slice implemented; module-cache write separation remains
Model: gpt-5.6-sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P2 developer workflow friction
Related:
- `src/sandbox.rs`
- `src/local_agent.rs`

## Observed behavior

`permission_mode=agent` の Temote session で Go repository の通常テスト:

```text
go test ./...
```

を実行すると、コードを評価する前に既定の Go build cache が read-only のため失敗した。

実測:

```text
open /home/hirohito-fujita/.cache/go-build/...: read-only file system
pattern ./...: open /home/hirohito-fujita/.cache/go-build/...: read-only file system
```

repository 配下の絶対 path を明示して:

```text
GOCACHE=/home/hirohito-fujita/src/gh-git/.cache/go-build go test ./...
```

とすると toolchain は起動できた。

さらに `go build` は `GOCACHE` だけを workspace-local にして exit 0 になったものの、既定 module cache に対して次の read-only warning を出した。

```text
go: writing stat cache: open /home/hirohito-fujita/go/pkg/mod/cache/download/...tmp: read-only file system
```

`GOCACHE` と `GOMODCACHE` の両方を workspace-local の絶対 path にすると、stderr なしで build が成功した。

## Problem

workspace-write sandbox でソースへの write は許可されていても、主要 language toolchain が通常利用する cache path が writable でないため、利用者・agent が language ごとの環境変数回避策を毎回知る必要がある。

これはコードの test failure と sandbox/environment failure を混同させ、developer UX を悪化させる。

## Goal

sandbox/path containment を維持したまま、supported development toolchains の ephemeral build/cache directory を安全な writable location へ自動的に向けられるようにする。

## Possible direction

- session / local-agent invocation ごとに workspace 外へ漏れない専用 cache root を Temote が用意する。
- `XDG_CACHE_HOME` 等の一般的な標準に寄せ、必要な language-specific variable だけ bounded allowlist で派生する。
- Go なら `GOCACHE` が絶対 path 必須である点と、`GOMODCACHE` も build 中に書き込み対象になる点を考慮する。
- cache を repository の tracked tree に混ぜず、session-owned temp/cache area に置く方を優先する。

## Acceptance criteria

- [x] agent-mode の通常 sandbox command に private `GOCACHE` を自動設定し、host `~/.cache/go-build` への write を要求しない。
- [ ] 素の `go build ./...` でも `GOCACHE` / `GOMODCACHE` の read-only warning/error を出さない。
- [x] writable cache は既存 sandbox の temporary root 内に限定し、新しい writable host root を追加しない。
- [x] cache path は secrets や host-global developer state を expose しない。
- [x] repository cleanup / git status を不要な `.cache/` artifact で汚さない。
- [ ] Go 以外へ拡張するときも toolchain ごとの ad-hoc HOME write 許可ではなく共通 cache policy で扱える。
- [ ] error 発生時は test failure と cache/sandbox failure を区別して報告できる。

## Implementation (2026-09-16)

通常の sandbox command ごとに Temote-owned private temporary cache root を作成し、`XDG_CACHE_HOME=<private>/xdg` と `GOCACHE=<private>/go-build` を設定する。sandbox は既に `/tmp` / `TMPDIR` を temporary root として writable にしているため、新しい filesystem capability は追加しない。`HOME` は従来どおり保持する。

`GOMODCACHE` はこのsliceでは意図的に変更しない。Go module cache は build cache だけでなく dependency store を兼ね、空のprivate directoryへ切り替えると network-disabled/offline execution が既存のread-only module contentsを再利用できなくなるためである。host module contentsをread-onlyのまま利用しつつ、`cache/download` 等のwrite metadataだけをprivate overlayへ分離する設計が必要。

Verification:

- `sandbox::generic_tests::preserves_home_for_login_shells`: PASS。
- `sandbox::generic_tests::generated_standard_cache_paths_stay_below_private_root`: `noprop` 1,024 cases PASS。
- `sandbox::generic_tests::command_cache_directory_is_private_and_removed_on_drop`: PASS（0700、drop後cleanup）。
- `GOMODCACHE` を自動設定していないことも unit test で固定。
- `cargo clippy --all-targets --all-features -- -D warnings`: PASS。
- `just sandboxed-check`: PASS / exit 0。
