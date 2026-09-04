# TEMOTE-02: 作業の申告状態を保存する、scope付きcheckpoint

- Status: Open
- Date: 2026-09-05 (Asia/Tokyo)
- Priority: P1
- Baseline: `8d5538314a8eb42ed5538d8b4c99c2514011df61` (`main`)
- Depends on: [TEMOTE-01](20260905-01-session-job-discovery.md)
- Next: [TEMOTE-03](20260905-03-read-only-work-handoff.md)

## 目的

会話が変わっても「何を実装したか、何を検証したと報告したか、次は何か」を読み直せるようにする。
job_listのlive情報と混同しないよう、保存する状態はすべてclient_reportedと明示する。
単なる実行成功からverifiedを自動設定しない。

## 読む範囲・変更範囲

- `AGENTS.md`, `src/config.rs`: state_dir、load_session、canonical_directory、metadataのnofollow/atomic write。
- `src/mcp.rs`: tool定義、call_tool、既存approvals::requestの使い方。
- 新規 `src/checkpoints.rs`、module登録の `src/main.rs`。
- `gateway/src/protocol.js`, `gateway/test/protocol.test.mjs`, 生成contract、usageドキュメント。

大きなmcp.rsの整理を先に始めない。保存とvalidationはcheckpoints.rsに閉じ込める。

## 固定するtoolと保存形式

- `checkpoint_save({session_id, checkpoint_id?, expected_revision, checkpoint})`
  - 新規はcheckpoint_id省略、expected_revision=0。serverがUUID v4を発行する。
  - 更新は既存IDと現在のrevisionが必須。不一致はCHECKPOINT_CONFLICT。
- `checkpoint_load({session_id, checkpoint_id})`: readonly。同じcanonical cwdに属する記録だけ読める。
- tool schemasとserde型はunknown fieldを拒否する。

clientが渡せるcheckpointは次の形に限定する（フィールド名も固定）。

```json
{
  "title":"resume validation",
  "base_commit":"0123456789012345678901234567890123456789",
  "steps":[{"id":"install","description":"installer validation","reported_status":"in_progress"}],
  "checks":[{"step_id":"install","name":"clean-install","reported_result":"not_run","commit":null}],
  "next_step_id":"install"
}
```

- reported_status: pending/in_progress/implemented/verified/blocked。
- reported_result: pass/fail/not_run。verifiedのstepには1つ以上のcheckが必要で、
  そのstepの全checkがpassかつcommitが非nullのbase_commitと一致すること。
  これは申告の整合性検査であり、実際に検証した証明とは呼ばない。
- step.id/check.nameは1..64文字のASCII英数・ハイフン・アンダースコア。step.idは重複不可。
- title/descriptionは最大256 UTF-8 bytes。steps/checksはそれぞれ最大64件。
- base_commit/check.commitはnullまたは40/64桁のhex。checks.step_idとnext_step_idは存在するstepを参照する。
- server envelope: schema_version=1、checkpoint_id、revision、scope_cwd、
  origin_session（id/started_at/process_id）、source="client_reported"、checkpoint。
  これらserver fieldはclientから上書きできない。

## 保存と権限の設計

server管理の `config::state_dir()/work-checkpoints/<UUID>.json` に保存する。
UUIDをparseしてcanonical表記に直し、任意path・root引数・環境変数による保存先指定は受け付けない。
scope_cwdは現在のvalidated session.cwdからserverが設定する。異なるworktreeは別scopeとする。

この専用toolが書けるのは上記のserver metadataだけ。通常セッションの一般コマンドやwrite_fileに
server state_dirへの権限を追加しない。normalでは保存前に `approvals::request` を通し、既存yoloの意味を維持する。
承認・activityログにはtool名と件数だけを出し、title/description/checkpoint本文を転載しない。
loadと後続の一覧ではscopeの一致を読み出し後・内容返却前に確認し、別scopeは汎用not-foundにする。

## 実装手順

1. 型とvalidationをpure helperで実装し、storageはbase directoryを注入できる設計にする。
2. 新規はcreate_new。更新はUUID単位のlock下でscope/revisionを再読込して比較する。
   macOS/Linux共通のlibc flock相当を使い、LOCK_NBで競合時はbusyを返す。renameだけをCASと見なさない。
   管理directory・lock・対象ファイルのsymlinkを拒否し、directoryは0700、fileは0600にする。既存の安全なmetadata処理を参考にする。
3. 最大64KiBでbounded read/write。JSON不正・不明schema version・read失敗を空の新規データ扱いにしない。
   同directoryのcreate_new一時ファイル→書込→flush/sync→renameを行い、失敗時は前の記録を保持する。
   lockは例外時にもRAIIで解放する。testで実ユーザーのstate_dirを使わない。
4. 成功後にrevisionを1増やした保存済みenvelopeを返す。deny/validation失敗時は書き込まない。
5. 後続issue用に `list_for_scope(session)` を追加。UUID名の記録だけ、最大128ファイルを走査し、
   一致するscopeのid/revision/titleだけ返す。上限時はtruncatedを返す。
   走査順はファイル名で固定。scopeを確認できない壊れた記録は候補から除外してincomplete=trueにする。
   その名前・件数・内容や別scopeの情報は返さない。
6. gateway定義・contract生成・docsを同時に揃える。raw argv、stdout/stderr、環境変数、secret値、
   子MCPの応答、会話全文を自動保存する経路を作らない。自由文にも秘密を書かないことを利用手順に明記する。

## 必須の受け入れテスト

- 同scopeの別sessionから保存→再読込でき、別cwd/worktreeからは読めない。
- 同revisionを使う2 writerのうち1つだけ成功する。lock競合とrevision conflictを区別する。
- 不正JSON、不明version、サイズ超過、symlink、無効UUID、重複step/存在しない参照を拒否する。
- 書込失敗で前のJSONが保存され、temporary fileとlockが残らない（lock用の通常file自体は残ってよい）。
- normalの承認拒否でディスク不変。contentのsentinelが承認/activityログに現れない。
- verifiedには同commitのpass checksを要求するが、返却sourceは常にclient_reportedのまま。
- processを作り直してloadできる。jobs、環境、credential、permissionを復元しない。

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

保存→競合→再読込の具体例とtest結果を記録する。新規DB/ネットワーク依存は追加しない。
利用中のserver upgrade、secretの永続化、Gitの自動commit/push、検証コマンドの自動実行は対象外。
