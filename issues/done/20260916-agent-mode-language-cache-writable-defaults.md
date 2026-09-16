# agent mode で標準 language build cache が read-only になり通常の test が失敗する

Status: done / private build cache + offline Go module-cache remap implemented
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
- [x] 素の `go build ./...` でも `GOCACHE` / `GOMODCACHE` の read-only warning/error を出さない。既存host module download cacheがある場合はprivate `GOMODCACHE` + read-only `file://` GOPROXYへ自動remapする。
- [x] writable cache は既存 sandbox の temporary root 内に限定し、新しい writable host root を追加しない。
- [x] cache path は secrets や host-global developer state を expose しない。
- [x] repository cleanup / git status を不要な `.cache/` artifact で汚さない。
- [x] Go 以外へ拡張するときも toolchain ごとの ad-hoc HOME write 許可ではなく共通 cache policy で扱える。private command cache root + `XDG_CACHE_HOME` を共通policyとし、language-specific mappingだけをboundedに追加する。
- [x] error 発生時は test failure と cache/sandbox failure を区別して報告できる。cache setupはchild開始前のparent-side error、test/build non-zeroはchild outputとして返る。activity上のより細かなtyped分類は `20260916-sandbox-setup-failure-misclassified-as-child-failed.md` に分離した。

## Implementation (2026-09-16)

通常の sandbox command ごとに Temote-owned private temporary cache root を作成し、`XDG_CACHE_HOME=<private>/xdg` と `GOCACHE=<private>/go-build` を設定する。sandbox は既に `/tmp` / `TMPDIR` を temporary root として writable にしているため、新しい filesystem capability は追加しない。`HOME` は従来どおり保持する。

`GOMODCACHE` は空のprivate directoryへ単純切替するとoffline executionが既存dependencyを見失うため、Go自身が既に持つfile-proxy互換layoutを利用する。`$HOME/go/pkg/mod/cache/download` が通常directoryとして存在する場合だけ、これをcanonicalizeした read-only `file://` GOPROXY として公開し、`GOMODCACHE=<private-command-cache>/go-mod` へ展開・metadata writeを行わせる。host module cacheへのwrite capabilityは追加しない。host download cacheが存在しない、symlink leafである、または安全なfile URLへ変換できない場合はremapをskipして従来挙動を維持する。

file URLはreserved byteをpercent-encodeする。private module cacheはcommand cache root配下、mode 0700。Goは展開module directoryをread-only化するため、`CommandCacheDir` cleanupはprivate root内のdirectoryだけowner-writableへ戻してから削除する。symlinkはfollowせず、外部targetのpermissionを変更しない。

Verification:

- `sandbox::generic_tests::preserves_home_for_login_shells`: PASS。
- `sandbox::generic_tests::generated_standard_cache_paths_stay_below_private_root`: `noprop` 1,024 cases PASS。private `go-mod` もroot containmentを確認。
- `sandbox::generic_tests::generated_go_file_proxy_urls_escape_reserved_path_bytes`: `noprop` 1,024 cases PASS。
- `go_module_cache_*`: 2/2 PASS。host download cacheありではprivate store + file proxy、なしでは既存挙動を維持。
- `command_cache_*`: 2/2 PASS。read-only module directoryを含んでもdrop cleanupでき、symlink targetを変更しない。
- 実Go probe: `github.com/google/uuid v1.6.0` を既存 `$HOME/go/pkg/mod/cache/download` から `GOPROXY=file://...` 経由で空のprivate `GOMODCACHE` へ取得して `go build ./...` exit 0。stderrはdownload noticeのみでread-only warningなし。
- probe cleanupでGo module directoryのread-only化を実測し、`issues/open/20260916-private-command-cache-readonly-cleanup.md` として記録して修正。
- `cargo clippy --all-targets -- -D warnings`: PASS。
- `cargo check --no-default-features --all-targets --locked`: PASS（既存dead-code warningのみ）。
- concurrent package-manager broker work が `gateway/contract/routed-tools.json` 等を変更中の working tree では gateway parity が一時FAILしたため、`HEAD=72e8f97` を clean treeへmaterializeし今回の `src/sandbox.rs` だけを適用して再検証。`just sandboxed-check`: exit 0。lib 110/110、activity job 5/5、activity coverage 3/3、upgrade transaction 40/40、upgrade coordinator 6/6、strict clippy、no-default check、gateway 2/2、`git diff --check` PASS。
- concurrent package-manager broker側のgateway contract同期完了後、combined working treeでも `just sandboxed-check`: exit 0。lib 110/110、activity job 5/5、activity coverage 3/3、upgrade transaction 40/40、upgrade coordinator 6/6、strict clippy、no-default check、gateway 2/2、`git diff --check` PASS。
- clean-tree gate の Linux nested sandbox / full binary-local socket / ignored supervisor E2E は **NOT RUN (host/CI gate)**。未実行をPASS扱いしない。
