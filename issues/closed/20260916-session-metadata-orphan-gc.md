# session metadata の `invalid_orphan` が大量滞留しても safe cleanup path がない

Status: superseded by bounded implementation packet
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 developer workflow friction
Type: session lifecycle / retention / maintenance

## Observed

active normal session から `temote-mcp doctor` を実行したところ、session metadata check 自体は PASS 扱いだが、次の実データが表示された。

```text
session metadata: entries=6273 json=3137 state=3136 other=0 retained_terminal=118 safely_prunable=0 invalid_orphan=3003
```

つまり全 pair の大半に相当する 3003 件が `invalid_orphan` と分類されているが、retention maintenance の `safely_prunable` には1件も入っていない。

既存 `issues/done/20260911-session-forget-stale-metadata.md` は、**既知の valid session ID 1件**を `temote-mcp session forget <id>` で安全に削除する機能を実装済み。ただし同 issue では broad purge は明示的 non-goal で、retention policy も変更していない。

## Current implementation

`build_retention_plan()` は metadata directory を走査し、以下のいずれかを `invalid_orphan_count` に加算した後、その entry/pair を retention candidate から除外する。

- `.json` / `.state` の片方しか存在しない
- filename stem が valid session ID ではない
- session metadata が読めない / stored ID が filename と一致しない
- lifecycle が読めない
- terminal lifecycle なのに `stopped_at` がない

したがって、invalid/orphan は diagnostic count には出るが、通常 retention maintenance では減らない。

## Problem

- `doctor` が thousands of orphan records を PASS line の数字だけで表示するため、operator は健全なのか maintenance が必要なのか判断しづらい。
- `session forget <id>` は pair が invalid / incomplete / malformed のケースを網羅する一括 maintenance API ではない。
- orphan accumulation が継続すると directory scan cost、diagnostic noise、backup/state size、operator investigation cost が増える。
- 一方で、invalid と分類したから即削除すると live session / upgrade handoff / partial durable write を誤削除する危険があるため、単純な `rm` や age-only purge は不適切。

## Goal

Temote-owned session state に対して、**なぜ orphan なのかを bounded に分類し、live/in-flight state を保護したまま安全に garbage collection できる supported maintenance path** を提供する。

## Proposed direction

1. `invalid_orphan` を reason 別に分類する。
   - missing_json
   - missing_state
   - invalid_id
   - unreadable_metadata
   - id_mismatch
   - unreadable_lifecycle
   - terminal_missing_stopped_at
   - other reviewed classes
2. `doctor` は aggregate count に加え reason counts を出し、一定量以上は WARN/FAIL policy を検討する。
3. local-owner-only の maintenance command を設計する。
   - 例: `temote-mcp session gc --dry-run`
   - default は dry-run / bounded plan
   - live socket probe、supervisor ownership、upgrade fencing、lifecycle lock を再利用
   - deletion は deterministic Temote-owned file names に限定
4. orphan class ごとに grace period / proof condition を定義する。partial write の直後を回収しない。
5. apply 時も preflight plan と current state を再照合し、drift した entry は skip/fail-closed にする。

## Security constraints

- workspace/worktree/project files を削除しない。
- session directory 全体を recursive delete しない。
- responsive live runtime、current supervisor ownership、in-flight lifecycle / upgrade restore target は絶対に回収しない。
- symlink/special-file substitution は既存 no-follow policy で拒否する。
- malformed entry の contents/path を arbitrary deletion target として信頼しない。
- public MCP に destructive GC を安易に公開しない。初期は local owner CLI を優先する。

## Acceptance criteria

- [ ] `doctor` が `invalid_orphan` を reason 別に説明できる。
- [ ] 現在 3003 件ある orphan 群について dry-run plan を安全に生成できる。
- [ ] safe class の orphan を bounded batch で cleanup できる。
- [ ] live/in-flight/upgrade-protected session は cleanup 対象にならない。
- [ ] cleanup 後に `session list/info`、supervisor restart、retention maintenance が回帰しない。
- [ ] partial pair、missing cwd、malformed metadata、symlink/special file、concurrent session start を含む regression tests を追加する。
- [ ] repeated lifecycle/test operation で orphan count が無制限に増え続けないことを確認する。

## Interruption checkpoint (2026-09-16)

現在 working tree に implementation draft があるが、main へ commit する gate は未達のため未コミットで保持している。

Draft scope:

- `src/cli.rs`
  - `temote-mcp session gc`
  - default dry-run
  - `--apply`
  - bounded `--limit 1..=1000`
- `src/config.rs`
  - validation 前の raw session metadata record read helper
- `src/doctor.rs`
  - orphan reason counts を表示
  - orphan がある場合 WARN + `session gc` guidance
- `src/main.rs`
  - session GC command dispatch
- `src/session_control.rs`
  - orphan reason classification
  - 24h grace
  - only `missing_json` / `missing_state` の reviewed subset を initial GC candidate 化
  - supervisor ownership / upgrade protection / live session probe
  - apply 時の state revalidation / drift skip

Observed gate state:

- `git status`: 上記5 tracked files が dirty。別の既存 `.tmp/`, `.wt/`, issue files は作業外として保持。
- `cargo fmt --all -- --check`: **FAIL**。`src/cli.rs` の新規 GC tests に rustfmt 差分あり。
- dedicated `session_gc` / orphan GC regression tests: **未確認・不足**。現時点の source search では実装関数は存在するが、destructive apply path を直接検証する専用 test を確認できていない。
- commit / push: **未実施**。destructive maintenance implementation を formatting/test 未達の状態で main に入れない。

Resume requirements before commit:

1. 今回の5ファイルだけを rustfmt し、他作業者差分を変更しない。
2. dry-run が filesystem mutation 0 である test を追加。
3. `missing_json` / `missing_state` の grace-period boundary test を追加。
4. live / supervisor-owned / upgrade-protected session が candidate にならない test を追加。
5. symlink / special file / malformed metadata / metadata ID mismatch を削除しない test を追加。
6. apply preflight 後に state drift / concurrent session start が起きた場合 skip/fail-closed になる test を追加。
7. bounded limit と deterministic ordering を test。
8. cleanup 後の session list/info, supervisor restart, retention maintenance regression を確認。
9. `cargo fmt --all -- --check`, strict clippy, no-default check, relevant tests, `git diff --check`, `just sandboxed-check` を実行。
10. host/CI-only tests は未実行なら `NOT RUN (host/CI gate)` と明記。
11. diff review / scope review 後、上記5ファイル + 本Issueだけを stage して commit / push。

## 2026-09-16 polishing disposition

The earlier dirty-tree checkpoint is no longer current (`main` is tracked-clean apart from pre-existing `.tmp/`/`.wt/`). Fresh implementation must start from current main using `issues/polished/20260916-session-orphan-gc.md`; do not resurrect the stale five-file draft blindly.
