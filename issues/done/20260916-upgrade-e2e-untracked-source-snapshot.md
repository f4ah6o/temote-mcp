# Upgrade reconnect E2E が未コミット新規 source file を target snapshot に含められない

Status: done
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P2 developer workflow friction
Type: process E2E / test fixture

## Observed

remote upgrade / activity 統合作業の commit 前 working tree で次を実行した。

```text
cargo test --test upgrade_reconnect_e2e -- --ignored --test-threads=1
```

`direct_http_upgrade_reconnects_after_real_ingress_replacement` は runtime/network assertion に到達する前に、distinct target build で失敗した。

```text
error[E0583]: file not found for module `render`
 --> src/activity.rs:4:1
  |
4 | pub mod render;
```

`copy_source_tree()` は security/reproducibility のため `git ls-files -z` で tracked files だけを bounded copy する。今回の working tree には新規 production files `src/activity/render.rs`, `src/activity_runtime.rs`, `src/upgrade_coordinator.rs` がまだ未コミット/未追跡のため、source module declaration はコピーされても module body が distinct target tree に存在しない。

current repository の index を変更・stage・commitすれば回避できるが、実装検証のためだけに developer state を変更するのは望ましくない。今回の依頼でも commit/push は許可されていない。

## Goal

tracked-source safety boundaryを維持しつつ、未コミット新規 source を含む feature branch / working tree でも process-boundary upgrade E2E を commit 前に実行できる supported path を用意する。

## Constraints

- default CI/release path の `git ls-files` bounded snapshot を broad recursive copy に置き換えない。
- `.git`, `.tmp`, `.wt`, secrets, credentials, arbitrary untracked files を target snapshot に取り込まない。
- current repository の index/staging stateを test helper が変更しない。
- symlink/submodule/size/file-count safety checksを維持する。

## Candidate direction

- explicit opt-in test-only source manifest / `--include-working-tree-source <allow-listed-path>` 相当を検討する。
- include可能なのは repository-relative regular file かつ `src/` 等の狭い allow-listに限定し、既存 byte/file boundsを適用する。
- CIは引き続き tracked-only mode を使い、opt-in mode は local pre-commit verificationだけにする。

## Acceptance criteria

- [x] default E2E source snapshot は tracked-only のまま。
- [x] pre-commit local verificationで明示した新規 production sourceだけを安全に含められる。
- [x] current Git indexを変更しない。
- [x] symlink、outside path、oversize、unbounded untracked sourceを拒否する。
- [x] CI/release E2E の再現性と security boundary が回帰しない。

## Resolution (2026-09-16)

`copy_source_tree()` の default は `git ls-files --cached` のまま維持し、`TEMOTE_TEST_INCLUDE_UNTRACKED_SOURCE=1` の明示時だけ `--others --exclude-standard` を追加する。対象 path は `Cargo.toml`, `Cargo.lock`, `README.md`, `src/`, `skills/temote-mcp/SKILL.md` に限定し、既存の regular-file / file-count / byte bounds をそのまま適用する。

pre-commit 検証では distinct target `2026.8.1` の build が成功し、従来の missing `src/activity/render.rs` compile error は解消した。その後の E2E は outer Temote sandbox の Unix socket `EPERM` で停止しており、この別制約は `20260916-normal-session-ci-sandbox-friction.md` で追跡する。
