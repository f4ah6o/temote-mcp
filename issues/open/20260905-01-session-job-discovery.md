# TEMOTE-01: 会話をまたいで実行中ジョブを再発見するjob_list

- Status: Open
- Date: 2026-09-05 (Asia/Tokyo)
- Priority: P1
- Baseline: `8d5538314a8eb42ed5538d8b4c99c2514011df61` (`main`)
- Depends on: なし
- Next: [TEMOTE-02](20260905-02-scoped-task-checkpoints.md), [TEMOTE-03](20260905-03-read-only-work-handoff.md)

## 目的と現在の不足

`execute` / `start_command` のjob_idを前の会話から持ち越せないと、実行中なのに同じ処理を再実行しやすい。
現在 `src/mcp.rs` にsession所有のJobStateがあるが、公開される操作はpoll_job/stop_jobで、IDが必要である。
現在のsessionに残っているジョブを、安全な短い一覧から見つけられるようにする。

## 最初に読むファイル

- `AGENTS.md`（実装時にも必読）。
- `src/mcp.rs`: Job/JobCompletion/JobState、tools、call_tool、store_job、inspect_job、reap_jobs、render_output。
- `gateway/src/protocol.js`, `gateway/test/protocol.test.mjs`。
- `issues/done/20260831-gateway-contract-parity-ci.md`。

## 追加するtool契約

`job_list({session_id, limit?})`。limitは1..128、default=50、unknown fieldを拒否する。
readOnlyHint=true、destructiveHint=false、openWorldHint=false。
既存 `call_tool` のsession認証・load_sessionの後で処理する。

返却するtext JSONの例:

```json
{"jobs":[{"job_id":"00000000-0000-4000-8000-000000000001","status":"running"}],"truncated":false,"retention":"in_memory"}
```

statusはrunning/completed/failed/unknownの4値。
CachedJobResult::Successはcompleted、Errorはfailed、handle終了かつ結果なしはunknown。
ここでcompletedはコマンドの成功終了だけを意味し、製品要件やテスト内容の合格を意味しない。
結果本文、argv、command、stdout、stderr、環境変数、raw errorは一切返さない。

## 実装手順

1. `src/mcp.rs` にprivateなJobSummaryとsnapshot helperを追加する。
   jobsのmutex内でsession所有権を確認し、必要なID/enumだけをコピーする。
   既存のjobs→completionのlock順を守る。lock保持中のawait、join、stdout parseを行わない。
2. runningを先に、その後をjob_idの辞書順で安定sortする。limit適用前の件数からtruncatedを算出する。
   一覧によって完了結果を消費しない。stop_job後に消えたジョブを成功扱いで復活させない。
3. tools定義とcall_toolに新toolを接続する。session_idなしの例外は増やさない。
   他sessionのjob IDや件数を含めない。
4. `gateway/src/protocol.js` の定義にも同じschema/annotationsを追加し、既存ルーティングで転送する。
   gateway/testの不足しているsession_id拒否、hostへの転送、応答の契約テストを追加する。
5. Rust側のsnapshot生成テストで `gateway/contract/routed-tools.json` を更新する。
   既存toolのschema/annotationsをついでに変更しない。
6. `docs/usage.md` / `docs/usage.ja.md` に「再開時はjob_list→既存jobをpoll」を短く記載する。
   メモリ内の結果はreap/プロセス終了でなくなるため、空一覧を未実行の証拠と説明しない。

## 必須の受け入れテスト

- `job_list_is_session_scoped`: A/B sessionの混在状態で自分のjobだけを返す。
- `job_list_does_not_consume_completion`: 2回一覧を読み、その後pollしても元の結果が得られる。
- `job_list_redacts_command_and_output`: command・成功出力・失敗出力にsentinel secretを入れても一覧に現れない。
- `job_list_orders_running_first_and_reports_truncation`: 順序、limit=1/128、範囲外、不明field。
- `job_list_empty_is_not_execution_history`: reap後や新規processの空一覧で「未実行」「成功済み」を断定するfieldがない。
- 既存poll/stop所有権テスト、completed TTLテスト、gateway parityテストが通る。

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

変更ファイル・新toolの例・テスト結果を完了記録に残す。
ジョブの永続化、終了済みプロセスの再実行、supervisor upgrade、release/installはこのissueに含めない。
稼働中のtemoteを再起動して検証せず、テスト用のsession/一時ディレクトリを使う。
