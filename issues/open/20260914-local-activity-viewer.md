# Temote supervisor の activity を read-only で閲覧する local CLI

Status: open
Model: unknown
Created: 2026-09-14
Updated: 2026-09-14
Branch: research/20260914-local-activity-viewer

## 概要

Temote MCP の supervisor が session と operation の安全な構造化 activity を bounded に保持し、必要なときだけ同一ユーザーの local CLI が履歴を表示して follow できるようにする。
activity viewer は runtime owner にならず、approval console と分離した read-only attachment とする。

## 背景

Temote の session lifecycle owner は `temote-mcp supervisor` の1つであり、全 `RuntimeHandle`、session socket、durable lifecycle metadata、approval broker をこの process が所有する。
`temote-mcp session console` は supervisor の private Unix socket に接続する reconnectable attachment であり、console の EOF、Ctrl-C、PTY disconnect、terminal close は console だけを detach する。

既存の approval console は approval prompt の送受信を責務とする。
通常の MCP operation の進行状況、完了、失敗、取消、session lifecycle の変化を観測する用途には使えない。

既存の `approvals::activity` は別の目的で存在する。
`src/approvals.rs` の `Message::Activity` と `activity()` は session runtime の Unix socket へ一方向の通知を送り、runtime 側の `show_activity_for_session()` が既存の start 画面へ表示する。
これは画面が閉じているときに失われる best-effort 通知であり、supervisor の履歴、live subscriber、再接続時の replay を提供しない。

関連する判断と実装は次の文書に記録されている。

- [`src/cli.rs`](../../src/cli.rs)：root command と `session console` の parser
- [`src/session_control.rs`](../../src/session_control.rs)：local supervisor Unix socket、`ControlRequest::AttachConsole`、`run_console()`、`handle_console_attachment()`、approval broker
- [`src/supervisor.rs`](../../src/supervisor.rs)：`SessionSupervisor`、`RuntimeHandle` の所有、lifecycle transition、restart、upgrade fence
- [`src/mcp.rs`](../../src/mcp.rs)：MCP tool dispatch、Git、command、job、local agent、integration の operation
- [`src/local_agent.rs`](../../src/local_agent.rs)：agent 名、access mode、task size、approval metadata の bounded contract
- [`docs/managed-sessions.ja.md`](../../docs/managed-sessions.ja.md)：supervisor ownership と approval console の運用仕様
- [`20260825-http-supervisor-approval-console-reattach.md`](../done/20260825-http-supervisor-approval-console-reattach.md)：approval attachment を runtime owner から分離した経緯

## 現在のアーキテクチャ調査結果

### CLI と supervisor control socket

`src/cli.rs` は `noargs` による root command parser を持ち、`session console` は `SessionCommand::Console` として `src/main.rs` の `session_control::run_console()` に渡される。
root command には現時点で activity viewer がない。

`src/session_control.rs` の control protocol は newline-delimited JSON であり、control message は `MAX_CONTROL_MESSAGE_BYTES` で bounded に扱われる。
通常の request は1行の response を返して接続を閉じるが、`AttachConsole` だけは最初の attach response 後も同じ connection を保持して approval event と approval response を双方向に処理する。

supervisor socket は `config::supervisor_socket_path()` で per-user の socket directory に配置される。
supervisor 起動時に親 directory を `0700`、socket を `0600` としており、現在の local console と同じ same-user boundary を再利用できる。

### ownership と approval broker

`SessionSupervisor` は `RuntimeHandle` を session ID で保持し、start、stop、restart、permission mutation、automatic restart、upgrade fence を transition mutex と supervisor memory で直列化する。
`session console` はこの map や runtime を所有せず、`console_registration` を通して approval broker に現在の prompt sender を登録するだけである。

`handle_console_attachment()` は attach response を返した後に approval prompt を送信し、console から `allow` boolean を受け取る。
`run_approval_broker()` は現在の console へだけ prompt を配送し、console がない場合や queue が満杯の場合は approval を deny する。
この approval semantics は activity viewer から変更しない。

### 既存の activity 通知と MCP operation

`src/mcp.rs` の複数の tool handler は `approvals::activity()` を呼び、編集、Git、command、job、local agent、1Password、kintone などの通知を既存の session runtime へ送っている。
一部の helper は operation title に command や path を含め、`report_command_finished()` は bounded process output の一部を activity detail に変換するため、これらの free-form title/detail を新しい viewer の wire contract にそのまま流用できない。

`src/local_agent.rs` は task 本文を activity label に含めず、agent、access、cwd、task bytes、task hash を approval metadata で扱う設計を持つ。
この bounded なメタデータの考え方は参考にできるが、new activity event には absolute path、task 本文、child output を入れない。

### 既存の bounded state との違い

`src/evidence.rs` は session と canonical scope に束縛された一時的な output storage であり、`src/friction.rs` は command argv、output、prompt、environment を保存しない bounded な friction metadata である。
どちらも privacy boundary の実装例になるが、activity は operation の時系列を live 表示する別の in-memory state とする。
friction event や evidence reference を activity history の代用にしない。

## 問題

開発者は常時ログ画面や approval prompt を開いたままにせず、必要なときだけ Temote の現在の activity を確認したい。
現在は `session console` が approval prompt 専用であり、次の情報を安全に確認できる read-only CLI がない。

- どの session で operation が開始、実行中、approval 待ち、完了、失敗、取消になったか
- background job や `local_agent_run` がまだ running か
- Git、session lifecycle、child integration の直近の結果
- supervisor の restart や runtime failure による状態変化

既存の best-effort activity 通知は viewer 起動前の履歴を保持せず、画面が閉じた間の event を replay しない。
一方で raw stdout/stderr、MCP request、agent prompt、credential-bearing input を常時ログへ書き出す設計は、activity の利便性より大きな privacy と retention のリスクを生む。

## 目標

- `temote-mcp activity` で全 session の bounded な recent activity を表示し、その後の event を follow できるようにする。
- `temote-mcp activity <session-id>` で1つの session に絞り込めるようにする。
- `--tail N` と `--no-follow` で replay 件数と終了条件を制御できるようにする。
- activity を typed `ActivityEvent` として定義し、timestamp、session ID、operation、state、duration、safe summary を一貫した contract で扱う。
- supervisor memory の bounded ring buffer と bounded live broadcast を使い、遅い viewer が MCP operation を止めないようにする。
- viewer の起動、終了、再接続が runtime ownership、approval、sandbox、permission mode に影響しないようにする。
- same-user local Unix control socket を正本とし、public MCP endpoint、HTTP、gateway へ activity attachment を公開しない。
- supervisor restart をまたぐ durable activity history は v1 で採用しないことを明確にする。

## 対象外

- `session console` の approval prompt と `allow` response の仕様変更
- activity viewer からの approval、cancel、stop、restart、permission mutation、任意 command の実行
- MCP `tools/list`、public HTTP、Cloudflare gateway、host federation からの arbitrary activity stream の公開
- raw stdout/stderr、full command argv、full MCP request、full agent prompt、environment value のログ化
- 1Password secret/value、access token、API key、credential、Authorization header の activity 保存または表示
- 無期限 logfile、SQLite、session metadata への巨大な durable event history の追加
- activity を audit log、forensic evidence、billing metric として扱うこと
- `git_status` など現時点で存在しない MCP tool をこの issue の前提として追加すること
- Temote の permission mode、sandbox policy、network policy、yolo semantics、session/runtime ownership の変更

## 提案する方針

### CLI UX

root command として `activity` を追加し、`session console` とは別の責務として扱う。
v1 の canonical syntax は次のとおりとする。

```bash
# 全 session の recent activity を表示して follow
temote-mcp activity

# 1つの session のみ
temote-mcp activity sf
temote-mcp activity dagu

# 直近 50 件を replay して follow
temote-mcp activity --tail 50

# 直近 100 件を表示して終了
temote-mcp activity --tail 100 --no-follow
```

`--tail` の既定値は bounded な `100`、上限は `1024` とする。
指定値は session filter 後の件数として扱い、ring buffer の eviction により指定数未満しか残っていない場合はその事実を attach response または表示ヘッダーで示す。
follow が既定であり、`--no-follow` は replay 完了後に supervisor が end marker を送って接続を閉じる。

viewer の通常表示は次のような一行形式とする。

```text
11:58:02  sf    local_agent_run  started
11:58:04  sf    git_pull         waiting_approval
11:58:08  dagu  git_pull         completed       remote=origin
11:58:13  sf    local_agent_run  running         agent=codex access=workspace_write
11:58:21  sf    git_push         completed       remote=origin
11:58:23  sf    local_agent_run  completed       21.3s
```

表示側は event の control character、newline、CR、terminal escape を表示前に除去または可視化する。
v1 の CLI は人間向け表示に限定し、raw JSON output option は別 issue とする。

### ActivityEvent contract

viewer が受け取る event は `ActivityEvent` として次の fields を持つ。

| field | contract |
| --- | --- |
| `schema_version` | activity event schema の version。初期値は `1` とし、unknown version は表示せず fail closed にする。 |
| `sequence` | supervisor process lifetime 内で broker が付与する単調増加 sequence。replay と live event の順序、および gap の検出に使う。 |
| `operation_id` | 同一 operation の started、approval、running、terminal event を関連付ける opaque UUID。認可や replay cursor には使わない。 |
| `timestamp_ms` | broker が publish 時に付与する Unix epoch milliseconds。CLI は local timezone の `HH:MM:SS` へ変換する。 |
| `session_id` | session-bound event では supervisor が socket の runtime identity から付与する session ID。session-independent な supervisor event だけ `null` を許可する。 |
| `operation` | bounded な canonical name。MCP tool name または supervisor internal operation name を使用し、caller の任意文字列を許可しない。 |
| `state` | `started`、`waiting_approval`、`running`、`completed`、`failed`、`cancelled` のいずれか。approval denial は `failed` とし、summary は固定された denial classification にする。 |
| `duration_ms` | terminal state では operation の開始からの bounded duration、それ以外では `null`。秒表示は CLI が行う。 |
| `safe_summary` | typed allow-list から組み立てた一行の bounded summary。raw input、output、error body を受け付けない。空文字列を許可する。 |

event 自体は control protocol の既存 message limit を超えないようにする。
`safe_summary` は UTF-8 で最大 `512` bytes、event envelope は最大 `2048` bytes を目安とし、terminal control character と改行を許可しない。
`sequence` と `timestamp_ms` は supervisor が上書きするため、MCP process が時刻や順序を偽装しても activity ordering を壊さない。

event の状態遷移は次のように扱う。

- operation の受付時に `started` を発行する。
- local approval を要求する operation は prompt が approval broker に入った時点で `waiting_approval` を発行する。
- approval 後、または approval 不要の operation の実処理開始時に `running` を発行する。
- 正常終了は `completed`、入力検証、sandbox、approval denial、child failure、protocol failure は `failed` とする。
- `stop_job`、session shutdown、timeout など Temote が処理を中断した場合は `cancelled` とする。

短時間の operation は `started` から直接 terminal event へ進んでもよい。
background job と local agent は request の受付 event と worker の terminal event を同じ `operation_id` で関連付ける。

### event coverage と safe summary

v1 は次の operation classes を activity の対象とする。

- session lifecycle：session start、stop、restart、permission change、crash、automatic restart
- approval：waiting、allow、deny、approval broker unavailable
- execution：`execute`、`start_command`、`poll_job`、`job_list`、`stop_job`、local-only の `without_sandbox`
- Git：`git_add`、`git_commit`、`git_fetch`、`git_pull`、`git_push`
- filesystem と work state：`read_file`、`get_image`、`list_directory`、`write_file`、`apply_patch`、checkpoint、handoff
- agent と child integration：`local_agent_run`、`codex_*`、`onepassword_*`、`kintone_*`、`recall`、friction related operation

purely informational な operation も記録する場合は operation name と completed state だけにし、結果本文を summary に含めない。
event coverage を実装上の都合で省略する場合は、対象 operation を docs に明記し、silent な partial coverage にしない。

safe summary は free-form string の redaction ではなく、operation ごとの typed constructor から作る。
許可する情報は operation name、状態分類、duration、bounded count、exit code、`remote=origin` のように検証済みで credential value を含まない識別子、agent/access mode、task byte count、opaque job ID などに限定する。
absolute cwd、任意 path、branch の未検証文字列、URL、query、request body、argument value は v1 の summary に入れない。

特に次の値は event、ring buffer、broadcast、CLI output、エラー summary のいずれにも入れない。

- 1Password secret/value、`op://` の secret value、service-account token
- access token、API key、credential、cookie、Authorization header
- full command stdout/stderr、child process output、full agent prompt、MCP request raw payload
- environment value、secret-bearing path、query、body、approval detail の本文

既存の approval summary、`child_env` の sensitive name list、`friction` の operation metadata、bounded text validator は再利用してよい。
ただし既存の `approvals::activity()` の title/detail や `report_command_finished()` の output summary を、新しい `safe_summary` の入力に直接渡さない。
失敗表示は raw `anyhow` text ではなく、`approval_denied`、`sandbox_denied`、`timeout`、`child_failed`、`runtime_unavailable` などの固定 classification とする。

### supervisor broker と event ingress

`SessionSupervisor` が `ActivityBroker` を所有する。
broker は次の2つだけを持つ bounded state とする。

```text
ActivityBroker
  ├─ VecDeque<ActivityEvent>      bounded recent history
  └─ broadcast::Sender<ActivityEvent>  bounded live delivery
```

MCP process は supervisor object を直接参照できないため、operation event の ingress には既存 session Unix socket の data path を使う。
新しい typed `Message::ActivityEvent` または同等の `ActivityUpdate` を追加し、payload には caller が session ID や absolute path を指定できないようにする。
runtime は接続先の active session identity を使って session ID を束縛し、supervisor の broker へ転送する。
session instance replacement 中に古い runtime から届いた update は、既存の expected session instance 検証と同じ考え方で破棄する。

既存の human-readable `Message::Activity` は start 画面の互換通知として残すか、移行完了まで別 adapter へ渡す。
これを approval prompt の `Message` や `AttachConsole` の registration と統合しない。

supervisor が直接知っている session lifecycle、runtime crash、automatic restart、approval broker の状態は supervisor process 内で broker へ publish する。
MCP 側の generic dispatcher が同じ lifecycle event を重複発行しないように、各 event の authoritative owner を実装前に決める。

event の publish は operation result の成功条件にしない。
broker が一時的に利用できない、viewer が存在しない、subscriber が lag している場合でも、MCP operation、approval、session lifecycle の既存動作は従来どおり継続する。

### read-only control attachment

`AttachConsole` とは別に、たとえば次の control request を追加する。

```text
ControlRequest::AttachActivity {
    session_id: Option<String>,
    tail: usize,
    follow: bool,
}
```

session ID は既存の `validate_session_id()` で検証し、`tail` は `0..=1024` またはそれに相当する明示的な bounded range とする。
request に path、command、MCP arguments、credential、approval response を含めない。

`handle_control_connection()` は `AttachActivity` を one-shot request の dispatch と分け、専用の `handle_activity_attachment()` へ渡す。
activity handler は次の順序で動作する。

1. broker に subscriber を登録し、history snapshot の終端 sequence を同じ操作で取得する。
2. filter 後の直近 `tail` 件を activity envelope として送る。
3. `follow=true` なら snapshot 終端より後の live event を sequence 順に送る。
4. `follow=false` なら end marker を送り、writer を shutdown して終了する。

subscriber 登録と history snapshot の境界を broker 内で直列化し、replay と live event の間の取りこぼしと重複を防ぐ。
subscriber が lag した場合は、sequence の欠落範囲を表す bounded な `activity_gap` marker を送る。
viewer は gap を表示し、必要なら同じ CLI を `--tail N` で再実行する。

wire envelope は次のように固定する。

```json
{"type":"activity","event":{"schema_version":1,"sequence":812,"operation_id":"00000000-0000-0000-0000-000000000001","timestamp_ms":1780000000000,"session_id":"sf","operation":"git_pull","state":"completed","duration_ms":1832,"safe_summary":"remote=origin"}}
{"type":"activity_gap","after_sequence":820,"dropped":3}
{"type":"activity_end","history_truncated":false}
```

既存 control protocol の version/capability check と整合させ、wire contract を変更する場合は `CONTROL_PROTOCOL_VERSION` または capability field を更新する。
旧 supervisor と新 CLI の組み合わせが activity request を黙って無視しないように、incompatible version は attach 前に fail closed にする。

activity handler の writer が停止した場合は、その attachment task だけを bounded timeout で終了する。
writer の backpressure、slow terminal、malformed viewer input が listener、broker、session runtime を止めないようにする。

### security boundary

activity viewer は supervisor socket の same-user local boundary の中だけで動く。
socket directory の `0700`、socket の `0600`、既存の socket path validation、control message size limit を維持する。
`host_id` は routing identity であり credential ではないため、activity authorization の代用にしない。

activity は read-only であり、attach 後の connection から受け取るデータは viewer disconnect の検知にだけ使う。
viewer の request を `dispatch_request()` の mutating branch に流さず、approval broker、session map、permission state、sandbox state へ write operation を追加しない。

public MCP `tools/list`、HTTP route、gateway routed-tool contract、gateway agent protocol に activity stream を追加しない。
認証済み public client が arbitrary session の activity を取得できる経路を作る必要性は v1 では弱く、local same-user CLI を正本とする。

### retention と backpressure

第一候補は supervisor process memory 内の global ring buffer とする。
初期値の目安は次のとおりである。

- `MAX_ACTIVITY_EVENTS = 4096`
- `MAX_ACTIVITY_EVENT_BYTES = 2048`
- `MAX_ACTIVITY_SUMMARY_BYTES = 512`
- `MAX_ACTIVITY_TAIL = 1024`
- live broadcast channel capacity は event 数で bounded にする

ring が満杯になったら最古の event を evict する。
必要なら global event 数に加えて session ごとの上限を設けるが、少なくとも global count と total bytes の両方を上限化し、1つの noisy session が unbounded state を作れないようにする。

event producer は broker lock や broadcast receiver の読み取りを待たず、publish 失敗や no subscriber を best-effort として扱う。
slow viewer には broadcast の lag を gap marker で知らせ、event delivery のために MCP operation を block しない。
attachment writer 自体にも bounded write timeout を設け、端末が出力を読まない場合は viewer だけを detach する。

activity event は disk、session metadata、upgrade restore plan、friction store、evidence store へ保存しない。
supervisor restart、same-PID upgrade、process crash では history は消え、active session runtime と approval semantics だけが既存の lifecycle contract に従って継続または復旧する。
supervisor restart をまたぐ durable history が必要になった場合は、secret retention、schema migration、access policy を別 issue で設計する。

### disconnect と reconnect

- Ctrl-C、stdin EOF、terminal close、control socket close は activity subscriber だけを破棄する。
- viewer の detach は session stop、approval denial、permission change、job cancellation を発生させない。
- 複数 viewer はそれぞれ独立した subscriber とし、後から attach した viewer が既存 viewer を置き換えない。
- `--no-follow` は history end marker 後に exit code `0` で終了する。
- supervisor が終了または upgrade した場合、follow connection は EOF で終了する。
- v1 は supervisor を起動したり無限 retry したりせず、再接続は同じ CLI を再実行して新しい bounded tail を取得する。
- supervisor restart 後に古い sequence を replay cursor として解釈しない。必要なら attach response の supervisor generation を表示用に返すが、v1 で durable resume は提供しない。

## 受け入れ条件

- [ ] `temote-mcp activity`、`temote-mcp activity <session-id>`、`--tail N`、`--no-follow` が parser、help、CLI dispatch に追加され、`--tail` が bounded に検証される。
- [ ] 引数なしは全 session、session ID 指定は exact match の session filter になり、invalid session ID や unknown option を fail closed する。
- [ ] viewer 起動時に recent history を replay し、既定では live event を follow し、`--no-follow` では history end marker 後に終了する。
- [ ] replay と live delivery の境界で event の重複や取りこぼしがなく、broadcast lag は bounded な gap marker として観測できる。
- [ ] `ActivityEvent` が schema version、sequence、operation ID、timestamp、session ID、operation、state、duration、safe summary を持ち、指定した state transition と terminal duration を満たす。
- [ ] session lifecycle、approval、Git、command/job、local agent、filesystem mutation、1Password/kintone/Codex などの対象 operation が canonical operation name で観測できる。
- [ ] `started`、`waiting_approval`、`running`、terminal state の event が、代表的な approval-required operation と background operation で正しい順序になる。
- [ ] event publish が viewer 不在、broker capacity、slow subscriber、viewer disconnect によって MCP operation、approval、session lifecycle の結果を失敗させない。
- [ ] activity summary、event JSON、ring buffer、broadcast、CLI output、エラー classification のどこにも secret/value、token、credential、Authorization header、full stdout/stderr、full prompt、raw request、environment value、credential-bearing path/query/body が現れない。
- [ ] typed safe summary が operationごとの allow-list と byte/control-character limit を通り、raw `approvals::activity()` detail や raw error text を直接表示しない。
- [ ] `AttachActivity` が `AttachConsole` と別 handler、別 state、別 protocol branch であり、activity viewer が approval response を送信または処理できない。
- [ ] control socket は既存の same-user `0700`/`0600` boundary と bounded line protocol を維持し、activity request に path、command、MCP payload、credential を受け付けない。
- [ ] public MCP `tools/list`、public HTTP、gateway routed-tool contract、gateway agent から activity attachment を取得できない。
- [ ] viewer の Ctrl-C、EOF、terminal close、socket close 後も active session/runtime が生存し、後から再実行した viewer が新しい tail を取得できる。
- [ ] supervisor restart または upgrade で in-memory history が消えること、durable activity file が作られないこと、session/runtime ownership が変わらないことを確認できる。
- [ ] approval semantics、permission mode、sandbox policy、network policy、yolo session の動作が activity 機能導入前後で変わらない。
- [ ] 関連 docs に CLI、local-only boundary、retention、event privacy、disconnect/reconnect の仕様が記載される。

## テスト計画

### unit test

- `ActivityEvent` の serde、schema version、state transition、sequence、duration、event size、summary size、control-character rejection を検証する。
- ring buffer の oldest eviction、tail 件数、session filter、history truncation、supervisor generation 境界を deterministic test する。
- broadcast subscriber の lag、gap marker、no subscriber、slow writer を検証し、producer が subscriber を待たないことを確認する。
- command argv、Git remote、1Password locator/value、kintone arguments、environment、agent task、stdout/stderr、path に secret sentinel を入れ、serialized event と rendered output に残らないことを検証する。
- `Message::ActivityEvent` の session identity binding、malformed/oversized payload、stale runtime update rejection を検証する。
- `AttachActivity` の tail/session/follow validation、history/live ordering、no-follow end marker、malformed viewer input、multiple subscriber、disconnect を検証する。
- 既存 approval tests に activity assertion を追加し、console 不在時の fail-closed、deny、session stop 時の prompt resolution が従来と同じ結果になることを検証する。

### integration と CLI test

- supervisor と複数 session を起動し、`activity --no-follow` が全 session の bounded history を表示することを確認する。
- `activity sf` が `dagu` の event を表示せず、`--tail N` が filter 後の newest event を返すことを確認する。
- Git、`execute`、background job、`local_agent_run`、approval-required operation を実行し、started、waiting、running、completed/failed/cancelled の順序を確認する。
- viewer を follow 中に Ctrl-C、stdin EOF、process close で終了させ、その前後で session socket probe と `session info` が active のままであることを確認する。
- 2つ以上の viewer を同時に attach し、一方の lag/disconnect が他方と runtime を停止させないことを確認する。
- supervisor を再起動または upgrade し、旧 viewer が EOF で終了し、古い history が再表示されず、active session は既存の lifecycle contract どおりであることを確認する。
- public MCP `tools/list`、gateway contract snapshot、HTTP endpoint へ activity request が出現しないことを確認する。

### repository checks

実装後は repository の通常 gate を実行する。

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
git diff --check
```

control protocol または共有 protocol の変更が gateway contract に影響する場合は、gateway test と snapshot check も実行する。

## リスク

既存の free-form activity detail を typed event に移行する途中で、command output、path、prompt、credential が新しい viewer へ漏れる可能性がある。
operation ごとの constructor、固定 error classification、secret sentinel test を必須にし、redaction だけに依存しない。

MCP process、session runtime、supervisor の3つの process boundary をまたぐため、session replacement 後に古い runtime の event が新しい session と混ざる可能性がある。
event ingress は接続先 runtime の session identity に束縛し、必要な場合は expected session instance を検証する。

viewer の表示が遅いと operation event の配送が詰まり、誤って session runtime や approval broker の待ち合わせへ波及する可能性がある。
broadcast、ring、event、writer の各 queue と write timeout を独立して bounded にし、publish を best-effort にする。

in-memory history は supervisor restart で失われるため、activity を audit log と誤認すると観測に欠落が生じる。
CLI と docs で「現在の bounded activity」であることを明記し、監査用途を別機能として扱う。

control protocol version を変更すると、旧 supervisor と新 CLI、または新 supervisor と旧 CLI の組み合わせが使えなくなる可能性がある。
既存の capability check、fail-closed upgrade、protocol version test を更新し、mixed generation が activity event を黙って捨てないようにする。

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- supervisor の current activity を同一ユーザーの `temote-mcp activity` から bounded replay/follow できる local read-only CLI を追加する。

## 注記

- 2026-09-14：本 issue は設計と実装分割を記録するためのものであり、この起票作業では実装、commit、push を行わない。
- 2026-09-14：approval console と activity viewer は同じ local supervisor socket boundary を使うが、control request、broker state、event framing、責務を分離する。

## 実装 slice

下位モデルが独立して実装と検証を進められるよう、次の順序で分割する。

### Slice 1: ActivityEvent contract と bounded broker

- `ActivityEvent`、state enum、schema version、operation ID、sequence、safe summary validator を追加する。
- supervisor memory 内の ring と bounded broadcast を追加し、tail、filter、eviction、lag marker の unit test を先に作る。
- この slice では control socket、MCP instrumentation、CLI output を変更しない。

### Slice 2: session runtime から supervisor への typed event ingress

- 既存 `Message::Activity` と分離した `ActivityEvent`/`ActivityUpdate` message を session protocol に追加する。
- runtime が socket identity から session ID を付与し、supervisor broker へ publish する経路を追加する。
- stale session instance、oversized payload、broker unavailable が operation result と approval semantics に影響しないことをテストする。

### Slice 3: local control protocol の activity attachment

- `ControlRequest::AttachActivity`、protocol version/capability、`handle_activity_attachment()`、activity envelope、end marker、gap marker を追加する。
- `AttachConsole`、approval registration、approval response path を変更せず、replay/live ordering と disconnect を integration test する。

### Slice 4: `temote-mcp activity` CLI viewer

- `src/cli.rs` と `src/main.rs` に root command、`SESSION_ID`、`--tail`、`--no-follow` を追加する。
- `session_control.rs` に local attach client、bounded line reader、human-readable renderer、Ctrl-C/EOF handling を追加する。
- parser/help、filter、no-follow、terminal control sanitization、supervisor unavailable の error path を test する。

### Slice 5: operation instrumentation と approval state

- `mcp.rs` の dispatcher と worker/job path に operation scope を導入し、Git、execute、background job、filesystem mutation、local agent、Codex、session lifecycle を typed event へ移行する。
- 1Password、kintone、child MCP、checkpoint、recall、friction の summary を operation-specific allow-list で追加する。
- `started`、`waiting_approval`、`running`、terminal state の correlation を確認し、既存 human-readable activity は互換 adapter として必要な期間だけ維持する。

### Slice 6: privacy、docs、full acceptance

- secret sentinel、raw output/prompt/request、credential-bearing path/query/body、control-character の regression test を追加する。
- [`docs/managed-sessions.ja.md`](../../docs/managed-sessions.ja.md) と対応する English docs に CLI、local-only boundary、retention、reconnect、非監査性を記載する。
- approval console parity、public endpoint non-exposure、supervisor restart/upgrade、multiple viewer、slow subscriber の end-to-end acceptance を実行する。
