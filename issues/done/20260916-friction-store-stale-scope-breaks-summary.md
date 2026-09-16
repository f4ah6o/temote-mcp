# 削除済み historical scope が `friction_summary` / learning candidate 全体を壊す

Status: done
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 developer workflow friction
Type: friction learning / durability

## Observed

active normal session `temo` (`cwd=/home/hirohito-fujita/src/local-mcp`) で、現在の session の摩擦を確認するため `friction_summary` を呼んだところ、現在 cwd とは無関係な削除済み temporary scope により失敗した。

```text
cannot resolve /tmp/.tmpqM6O9Z: No such file or directory (os error 2)
```

同じ session で `learning_candidate_list` も同一エラーで失敗した。

現在の session 自体は `active` で、canonical cwd `/home/hirohito-fujita/src/local-mcp` は存在する。

## Root cause

`Store::events_for_session()` は次の順序で処理する。

1. current session scope を検証
2. `read_all()` で store 内の全 event を読み込む
3. その後に `event.scope_cwd == session.cwd && event.session_id == session.id` で filter

しかし `read_all()` -> `read_event()` -> `validate_event()` は、各 historical event の `scope_cwd` を `config::canonical_directory()` で再 canonicalize する。この API は path の現存を要求する。

そのため、過去の temporary repository / worktree / test fixture が正常に削除されただけでも、その event が retention store に残っている間は、現在 session と無関係でも `read_all()` が fail-fast し、`friction_summary`、`learning_candidate_list`、さらに `prune_locked()` を利用する新規記録処理まで影響し得る。

これは persisted event の integrity 検証と、「historical scope が現在も filesystem 上に存在すること」を混同している。

## Goal

historical friction event の security/integrity validation を維持しつつ、event 記録後に scope directory が削除される正常な lifecycle を許容する。

## Proposed direction

- write 時には current session cwd を canonical existing directory として検証し、その canonical absolute path を event に固定する。
- read 時には、既に保存された `scope_cwd` に対して existence-dependent canonicalization を要求しない。
- persisted path の validation は、absolute path、bounded length、normalization、forbidden malformed representation 等の existence-independent invariant に分離する。
- current session 用 query では、最低でも session ID / stored scope metadata で対象を絞った後に、現在必要な scope だけ current canonical scope と比較する。
- malformed/corrupt event を無視するのか store 全体を fail closed にするのかは、tamper detection と availability の trade-off を明示して決める。単に全 validation を削除しない。
- prune は stale/deleted scope event も時刻順に安全に削除できること。

## Implemented

write/current-session側の `validate_session_scope()` は従来どおり `config::canonical_directory()` を使い、scopeが**現在存在するcanonical directory**であることを要求する。一方、persisted eventの `validate_event()` は filesystem existence に依存しない `validate_persisted_scope()` へ分離した。

persisted scope は以下をfail-closedに検証する。

- absolute pathのみ
- 1..=4096 bytes
- NULなし
- `.` / `..` / repeated separator 等を含まないlexically normalized representation

event file自体の `O_NOFOLLOW`、owner-only mode、UUID filename/event ID一致、schema/size/identifier validationは変更していない。したがって historical cwd が正常に削除されても読み取れる一方、malformed/corrupt eventをtrusted scopeとして扱わない。

## Security constraints

- symlink event files、owner-only permission、event ID/file-name consistency、schema/version、size limits は維持する。
- stale scope を許容するために arbitrary current path を trusted scope として扱わない。
- session/scope isolation を弱めず、別 session の event を現在 session の summary に混ぜない。
- malformed/corrupt event と、正当だが既に存在しない historical scope を区別する。

## Acceptance criteria

- [x] event 記録後にその original cwd を削除しても、別の active session の `friction_summary` が正常に返る。
- [x] 同条件で `learning_candidate_list` が正常に返る。
- [x] 同じ session ID/scope に属する historical event は、scope が削除済みでも retention/prune の対象として安全に処理できる。
- [x] deleted scope event が別 current scope の summary に混入しない。
- [x] malformed relative path、oversized path、event ID mismatch、symlink event、unsafe file mode 等は引き続き拒否される。
- [x] temporary worktree / tempdir を作成→event記録→削除→summary/candidate/prune の regression test を追加する。
- [x] stale event 1件で store 全体の current-session observability が利用不能にならない。

## Verification

- `cargo test --bin temote-mcp --all-features --locked friction::tests::`: **13/13 PASS**。
- `generated_persisted_scope_validation_matches_normalized_absolute_path_model`: `noprop` **1,024 cases PASS**。
- `deleted_historical_scope_does_not_break_current_summary_candidate_or_prune`: PASS。deleted scope eventを保持した状態で別current scopeのsummary/candidateが正常、retentionによるstale event pruneも成功。
- `persisted_event_integrity_checks_remain_fail_closed`: PASS。public mode / event ID mismatch / symlink eventを拒否。
- `cargo clippy --all-targets -- -D warnings`: PASS。
- `cargo check --no-default-features --all-targets --locked`: PASS（既存dead-code warningのみ）。
- `just sandboxed-check`: exit 0。lib 110/110、activity job 5/5、activity coverage 3/3、upgrade transaction 40/40、upgrade coordinator 6/6、gateway 2/2、`git diff --check` PASS。
- Linux nested sandbox runtime / full binary-local Unix-socket integration / ignored supervisor-process-boundary E2E: **NOT RUN (host/CI gate)**。
