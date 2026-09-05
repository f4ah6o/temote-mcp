# TEMOTE-04: session metadata蓄積でsession_listを壊さないbounded discoveryとretention

- Status: Open
- Date: 2026-09-05 (Asia/Tokyo)
- Priority: P1
- Baseline: `4ae3847cf64ec52a1f7b7e8b7bf7b28af88236dd` (`main`)
- Depends on: なし
- Related: [TEMOTE-01](20260905-01-session-job-discovery.md)

## 事象

長期間使ったhostで `session_list` が次のエラーになり、active sessionの発見に失敗する。

```text
session metadata directory exceeds 4096 entries
```

2026-09-05の実機再現では、session metadata directoryに4186 entriesが存在した。

- `.json`: 3508
- `.state`: 676
- `.tmp`: 2
- `temote-mcp session list`: inspect可能なsession viewは13件（active 4 / stopped 7 / crashed 2）

個別の `session_info` はactive sessionに対して正常であり、supervisor upgradeやsession runtimeの破損ではない。

## 原因

`src/mcp.rs::session_list` はdirectory entryを読むたびに、拡張子やsession IDを判定する前に `MAX_SESSION_METADATA_ENTRIES_SCANNED = 4096` を加算している。
そのため `.state` / `.tmp` / stale・legacy metadataもscan budgetを消費し、返却可能なsessionが少数でも4097件目で全体を失敗させる。

また現在の上限は別々に、

- scan: 4096 directory entries
- response: 256 sessions
- response bytes: 4 MiB

となっている。単に4096を増やすだけでは、履歴がさらに蓄積したhostで同じ問題が再発する。

`src/session_control.rs::list_session_views` はinspectに失敗したmetadataをskipできるが、directory全体を走査する。MCP側とCLI側で大量履歴時のsemanticsとboundが揃っていない。

## 目的

1. 古い/stale/legacy metadataが大量に残っていても、active session discoveryを失敗させない。
2. `session_list` のCPU / I/O / response sizeは引き続きboundedにする。
3. active sessionをhistoryの量やdirectory iteration順に依存させず、必ず優先して返す。
4. terminal session metadataを無制限に増やさないretention方針を持つ。
5. cleanupでactive sessionや復旧に必要なmetadataを消さない。

## 最初に読むファイル

- `AGENTS.md`
- `src/mcp.rs`: `session_list`, `next_session_scan_count`, `push_session_list_entry`, `session_list_entry`, `MAX_SESSION_*`
- `src/session_control.rs`: `ControlRequest::List`, `list_session_views`, `reconcile_stale_sessions`, supervisor session ownership
- `src/config.rs`: `sessions_dir`, `session_path`, `session_lifecycle_path`, metadata/lifecycle read-write
- `issues/done/20260825-session-lifecycle-supervisor-hardening.md`
- `issues/open/20260902-zero-downtime-supervisor-upgrade.md`

## 実装方針

### 1. active sessionsをhistory scanから分離する

`session_list` のactive session発見を、filesystem directory iterationだけに依存させない。
running supervisorが所有するsession setをsource of truthとして先に取得し、そのactive entriesを必ず結果へ入れる。

既存control protocolへboundedなactive-session snapshotを追加するか、同等にsupervisorのin-memory ownershipから取得する。
filesystem historyが壊れていてもactive session discoveryは成立させる。

active session数自体がresponse上限を超える場合だけは、明示的なbounded failureまたは契約済みのtruncationを返す。directoryのstale history件数を理由にactive sessionを落とさない。

### 2. historical entriesはbest-effortかつboundedにする

stopped/crashed historyはactive entriesの後に追加する。

- `.json` 以外をsession candidateとして数えない。
- invalid session ID、parse不能、inspect不能なlegacy/stale entryはactive discovery失敗へ昇格させない。
- response上限256件 / 4 MiBは維持する。
- directory iteration順に依存しない。historical entriesを返すなら `stopped_at` / `started_at` とsession IDで安定順序を定義する。
- 256件を超えるhistoryを「error」にせず、activeを保持したままbounded resultにする。既存array contractを変える必要がある場合はgateway/client compatibilityを先に確認する。

### 3. terminal metadata retentionを導入する

停止済みmetadataを無制限に残さない。

安全条件:

- active / starting / stopping sessionは絶対に削除しない。
- active socketがあるIDは削除しない。
- supervisor upgrade restore planが参照するsessionは削除しない。
- cleanup対象は、lifecycleがterminal (`stopped` / `crashed`) と確認できるmetadataに限定する。
- `.json` と対応する `.state` はpairとして扱う。
- parse不能・schema不明のmetadataを自動で破壊しない。必要ならdoctorでorphanとして報告する。

retention値は定数化し、少なくとも「最近のterminal sessionを一定数保持 + それより古い安全なterminal metadataをprune」というdeterministic policyにする。
時刻だけに依存するTTLより、件数上限も併用してdirectory growthを確実にboundする。

cleanupはsupervisor maintenance/startupなど既存ownershipが明確な箇所で行い、`session_list` というread operationの副作用として削除しない。

### 4. observability

`doctor` またはdebug diagnosticsで、secret/path内容を不必要に出さず次を確認できるようにする。

- total metadata entries
- `.json` / `.state` / other count
- retained terminal count
- safely prunable count
- invalid/orphan count

通常の `session_list` responseへ内部ファイル名やraw parse errorを混ぜない。

## 必須の受け入れテスト

- `session_list_survives_more_than_scan_limit_of_non_json_metadata`
  - 4096件を超える `.state` / `.tmp` があっても、active sessionを返せる。
- `session_list_prioritizes_active_sessions_over_large_history`
  - 4096件超のhistorical candidateと複数active sessionを混在させ、activeがすべて結果に入る。
- `session_list_skips_invalid_legacy_metadata_without_failing_active_discovery`
  - malformed JSON、unknown schema、invalid IDを含めてもactive discoveryは成功する。
- `session_list_remains_bounded_and_deterministic`
  - 256 entries / 4 MiB上限を破らず、同じ状態で安定した順序になる。
- `metadata_retention_never_removes_active_sessions`
  - active/starting/stopping、live socket、upgrade restore対象をcleanupしない。
- `metadata_retention_prunes_only_confirmed_terminal_pairs`
  - terminal `.json` / `.state` のみpolicyどおりpruneし、parse不能metadataは残す。
- `metadata_retention_bounds_repeated_ephemeral_sessions`
  - 多数の短命sessionを生成・停止してもmetadata directoryがpolicy上限へ収束する。
- CLI `temote-mcp session list` とMCP `session_list` でactive-session集合が一致する。

## 実機再現の回帰条件

次と同等の状態をfixture/temp directoryで作り、実ユーザーのstate directoryはテストで変更しない。

```text
total entries: 4186
.json: 3508
.state: 676
.tmp: 2
inspectable session views: 13
active: 4
```

この状態でMCP `session_list` が `session metadata directory exceeds 4096 entries` を返さず、少なくとも4 active sessionsを発見できること。

## 検証と完了条件

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
git diff --check
```

gateway contractに変更が入る場合はsnapshot/parity testも更新する。
完了記録にはretention policy、before/afterのmetadata件数、MCP/CLI両方の大規模fixture結果を残す。

実機の既存session metadataを手作業で削除して「直った」としない。まずコード側でactive discoveryをhistory蓄積から独立させ、その後安全なretentionを実装する。