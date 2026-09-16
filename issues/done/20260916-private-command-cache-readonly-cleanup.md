# private command cache cleanup が Go module cache の read-only directory で失敗する

Status: done
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 cleanup / developer workflow friction
Related: `issues/doing/20260916-agent-mode-language-cache-writable-defaults.md`

## Observed

Go module download cacheをhost read-only file proxyからprivate `GOMODCACHE`へ展開する実probeは `go build ./...` exit 0 になったが、その後probe専用cacheを通常の `rm -rf` で削除すると `github.com/google/uuid@v1.6.0/*: Permission denied` が発生した。

Goはmodule cacheの展開directoryを通常read-onlyにするため、現行 `CommandCacheDir::drop` の単純な `std::fs::remove_dir_all` も同じ条件で失敗し、エラーを無視してprivate cacheを残す可能性がある。

## Goal

Temote自身が作成したunique private command cache rootだけを対象に、read-only module directoryを含んでも確実にcleanupできるようにする。他pathやsymlink targetのpermissionは変更しない。

## Acceptance

- [x] private cache配下のread-only directory/fileを含むfixtureをdrop後に削除できる。
- [x] symlinkをfollowして外部targetのpermissionを変更しない。
- [x] cleanup対象はTemote-owned `CommandCacheDir` rootから広がらない。
- [x] deterministic gateがgreen。

## Implementation / verification

`CommandCacheDir::drop` は Unix で private root 内の実directoryだけを再帰し、owner `rwx` bitを戻してから `remove_dir_all` する。`symlink_metadata` / `DirEntry::file_type` を使いsymlinkは再帰しないため、外部targetのpermissionは変更しない。file自体をchmodする必要はなく、親directoryだけをremovableにする。

- `command_cache_directory_is_private_and_removed_on_drop`: PASS。0555のGo module相当directoryを含めてdrop後にroot消滅。
- `command_cache_cleanup_does_not_follow_symlinks`: PASS。外部target mode 0500を維持。
- clean-tree `just sandboxed-check`: exit 0。lib 110/110、gateway 2/2を含むrepository-local gate PASS。
- host/CI-only nested sandbox/full integration/E2Eは **NOT RUN (host/CI gate)**。
