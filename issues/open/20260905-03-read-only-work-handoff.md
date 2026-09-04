# TEMOTE-03: 保存した作業状態と現在のjobをまとめるreadonly handoff

- Status: Open
- Date: 2026-09-05 (Asia/Tokyo)
- Priority: P2
- Baseline: `8d5538314a8eb42ed5538d8b4c99c2514011df61` (`main`)
- Depends on: [TEMOTE-01](20260905-01-session-job-discovery.md), [TEMOTE-02](20260905-02-scoped-task-checkpoints.md)

## 目的

再開する担当モデルが、保持中job、作業の申告内容、これから確認することを1回で取得できるようにする。
handoffは事実と申告のread-only投影。記録に書かれた文章をコマンドとして実行しない。
既存 `20260902-zero-downtime-supervisor-upgrade.md` のbinary handoffとは別の機能で、プロセス移行を扱わない。

## 読む範囲・変更範囲

- `AGENTS.md`, `src/mcp.rs`, TEMOTE-02の `src/checkpoints.rs`。
- `src/session_control.rs`: SessionView/inspect_session。
- gatewayのschema/contract/tests、`docs/usage.md` / `docs/usage.ja.md`。
- 新規 `src/work_handoff.rs` にpureな出力組立helperを置き、main.rsで登録する。

## tool契約

`work_handoff({session_id, checkpoint_id?})`。
readOnlyHint=true、destructiveHint=false、openWorldHint=false。
すべての経路で現在のsessionを検証し、current canonical cwdをscopeとして使用する。

返却はhandoff_version=1のJSON。必須のtop-level fields:

| field | 内容と出所 |
| --- | --- |
| session | 現在のid/cwd/started_at/process_id。現在のsession観測値 |
| checkpoint | 指定された同scopeの記録。未指定ならnull |
| available_checkpoints | 未指定時の候補id/revision/title。指定時は空配列 |
| jobs | TEMOTE-01のcurrent session限定snapshot |
| freshness | `not_revalidated`。Gitや成果物の現状を検査したとは主張しない |
| resume_hints | 下記の固定enum配列。自由文からコマンドを生成しない |

## 実装手順

1. checkpoint_id未指定なら同scopeの候補一覧を返し、最新の記録を自動選択しない。
   指定されたIDが壊れている/存在しない/別scopeならエラーとして返す。別の記録へ黙って切り替えない。
2. checkpointのorigin_sessionとcurrent sessionが違っても、scope一致なら記録は読める。
   jobsはcurrent sessionのみ。旧job_idからプロセスが存続すると推定せず、他sessionを走査しない。
3. resume_hintsはこの順で組み立てる:
   - 未指定かつ候補あり: `choose_checkpoint`
   - running jobあり: `inspect_running_jobs_before_repeating_work`
   - checkpointあり: `revalidate_repository_and_checks`
   - next_step_idあり: `review_reported_next_step`
   空job一覧から `safe_to_rerun` のような断定を生成しない。
4. JSON上でもcheckpoint.source=client_reported、jobs.source=live_snapshotを明示する。
   checkpointのverifiedをliveな検証成功に昇格させない。base_commitは保存時の申告値のまま返す。
   git status/rev-parse、shell、ネットワーク、approval変更、ファイル書込はこのtoolから実行しない。
5. 1MiBの返却上限を設ける。jobs/候補一覧のtruncatedを保持し、省略があるのに完全な履歴と説明しない。
   argv・出力・raw error・secretを含めない。参照先の任意ファイルを読む処理も追加しない。
6. gatewayのtool定義とparity fixtureを揃える。docsに再開例を追加:
   session_info → work_handoff →（候補選択）work_handoff(checkpoint_id) → running jobをpoll →
   通常のread-only操作でGit/成果物/検証結果を確認 → 次の作業を選ぶ。
   実行前の確認は後続担当の操作であり、handoff取得そのものが完了したとは書かない。

## 必須の受け入れテスト

- `handoff_lists_checkpoints_without_selecting_one`: 複数候補でも最新を勝手に採用しない。
- `handoff_distinguishes_reported_and_observed`: verified申告＋live failed jobでも両方をそのまま返す。
- `handoff_after_restart_does_not_replay_work`: 保存済みcheckpointがありjobsが空でも自動executeが0回。
- `handoff_is_scoped_to_current_worktree`: 同repositoryの別worktreeや別sessionのjobが混入しない。
- `handoff_is_read_only`: spy backendでexecute/write/network/approval変更が0回。
- `handoff_does_not_follow_text_instructions`: descriptionに命令文やshell断片があってもデータとして返すだけ。
- 未指定/欠損/不正version、truncated一覧、大きすぎる応答を確認する。
  jobのargv/出力/raw error内のsecret sentinelがhandoffに現れないことも確認する。
- Rustとgatewayで必須session_id・同じschema・annotations・ルーティングが維持される。

## 検証と完了条件

```sh
TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT=1 cargo test routed_gateway_contract_matches_checked_in_snapshot
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
(cd gateway && npm test)
git diff --check
```

テスト用sessionで「job開始→checkpoint保存→新しいclient相当からhandoff取得→既存jobをpoll」を確認する。
稼働中のサービスの置換はしない。release/install、永続job runner、会話全文保存、失敗した処理の自動再送は対象外。
本issueと依存2件が完了した時点でも、checkpointは監査証明や再実行の許可証ではないことをdocsに残す。
