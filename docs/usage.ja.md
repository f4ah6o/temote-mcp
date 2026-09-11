# Temote MCP の使い方

[English](usage.md)

## session

local work では Temote の lifecycle supervisor を1つ起動し、別 terminal から named-root session を作成します。

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor

temote-mcp session start my-project --path src/my-project
temote-mcp session list
temote-mcp session info my-project
```

local approval input が必要な場合は `temote-mcp session console` を使います。この console を閉じる、または stdin EOF になっても runtime は停止せず、console だけが detach します。console 不在中の approval-required operation は fail closed します。

installed binary の更新後は `temote-mcp upgrade --dry-run` → `temote-mcp upgrade` で compatible な same-PID supervisor handoff と coordinated session restart/restore を行えます。credential value は永続化せず、restart context 不足または in-flight operation があれば中止し、planned session を全て確認してから成功を返します。handoff protocol 導入前の supervisor からは最初に手動 restart が1回必要です。

`session list` では durable な `starting` / `active` / `stopping` / `stopped` / `crashed` を確認できます。`session info` では working directory、permitted root、permission mode、timestamp、exit reason、last error を確認できます。死んでいる、または liveness が曖昧な socket を暗黙に active とは扱いません。manual restart は `temote-mcp session restart <id>` で行えます。自動 restart は現時点では有効化しません。restart は old full session instance を fence し、replacement の開始前に登録済み Codex runtime を shutdown します。replacement の開始に失敗しても old child runtime は残しません。

session discovery は active-first です。running supervisor が所有する session を bounded な historical metadata より先に返すため、履歴が蓄積しても active session が `session list` / MCP `session_list` から押し出されません。historical な stopped / crashed entry は list budget 内で deterministic な recent-first 順に返します。supervisor startup と periodic maintenance では、安全に terminal と確認できた metadata pair のうち最近512件を保持し、それより古い confirmed stopped / crashed pair だけを prune します。live、曖昧、malformed / orphan、supervisor upgrade restore plan で保護されている metadata は retention で自動削除しません。read-only listing と MCP fallback は cleanup を行いません。

`temote-mcp session forget <id>` は、terminal で non-live な1 session の Temote-owned durable state（metadata、lifecycle state、stale と確認済みの socket entry）を削除します。`stop` は後から `session list` / `session info` で参照できるよう metadata を保持し、`forget` は意図的に削除します。runtime socket probe が live を返した場合は無条件で拒否し、supervisor の lifecycle transition と直列化され、symlink や非 regular file の metadata target を拒否し、workspace、cwd、worktree には触れません。1 session の forget は他 session の retention policy を変更しません。

互換用に `cd ~/src/my-project && temote-mcp start my-project` も利用できます。これは起動中の local supervisor に current directory の session 作成を依頼します。`temote-mcp start my-project --yolo` は意図的に制限を外す local-only form として残します。

相対 path は session の working directory を基準に解決されます。

### HTTP から作る managed session

host で `TEMOTE_MCP_ROOTS` を lifecycle supervisor に設定して常駐させ、`temote-mcp up` は別 process として起動します。

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor
# 別 terminal/service
temote-mcp up
```

複数 root は区切り文字の ad-hoc list ではなく、supervisor process に JSON object で設定します。

```sh
export TEMOTE_MCP_ROOTS='{"src":"~/src","work":"~/work"}'
temote-mcp supervisor
```

client は `session_list` を確認し、必要なら `session_start(path="src/project")`、続いて `session_info` を呼びます。configured root 自体は canonicalize されるため、`~/src -> /Volumes/devstorage/Developer` のような host alias は利用できます。一方、その配下の symlink や `..` が canonical physical root の外へ解決される場合は拒否されます。roots 未設定時は HOME、`/`、cwd、repository cwd へ fallback しません。

`session_stop` で停止できるのは lifecycle supervisor が HTTP-owned として記録している session だけです。同じ supervisor process 配下でも local CLI / yolo session は remote `session_stop` から停止できません。HTTP managed session は常に non-yolo で、approval は `temote-mcp session console` に集約します。public session-bound tool は別途起動した yolo session 自体も拒否するため、remote access が unrestricted な local semantics を引き継ぐことはありません。stopped / crashed metadata は `session_list` / `session_info` から確認できますが、それ以外の session-bound tool は引き続き active socket を要求します。`temote-mcp down` が停止するのは HTTP origin と managed ingress child だけで、lifecycle supervisor と session は停止しません。repository checkout の `just up/down` は、この CLI command に委譲する開発用 wrapper です。

## 旧 always-on runtime の migration

古い repository checkout の `just up` は `temote-mcp serve` と `cloudflared` を sibling process として起動し、2つの PID を `~/.cache/temote-mcp/up.pids` に記録していました。現在の installed deployment は `temote-mcp up`、lock 付きの単一 `up.pid`、child-process ownership を使います。binary を置き換えても、すでに実行中の process 自体は新しくなりません。

current binary を install した後、一度だけ legacy runtime state を migrate します。

```sh
cargo binstall temote-mcp --force
temote-mcp migrate --dry-run
temote-mcp migrate
TEMOTE_MCP_ROOTS='src=~/src' temote-mcp supervisor
# 別 terminal/service
temote-mcp up --profile cloudflare
```

migration は legacy state file を安全に検証し、signal 前に live PID の process name を確認します。別 process へ PID が再利用されている場合は fail-closed です。削除するのは stale legacy state、停止するのは検証済みの旧 `temote-mcp serve` + `cloudflared` pair だけです。`public.env`、`tunnel-token`、session metadata、socket、別途起動した `temote-mcp start <session>` process は変更しません。legacy state が無い状態で `temote-mcp migrate` を再実行しても no-op です。

## 許可 root

通常 session では canonical な起動 directory が最初の permitted root です。local named-root selection で対象 project directory を決め、remote `session_start` は administrator が設定した named root 配下しか解決できません。通常 session は permitted root の外へ出る path、symlink target、command `cwd` を拒否します。

従来の inline `/permission ...` terminal command UI は detached runtime の owner ではなくなったため、第一段階の supervisor control surface には載せていません。権限を広げる変更ではなく、runtime は persisted permitted root のまま fail closed します。

## command

`execute` は shell を介さず argv を実行します。通常 session では network 無効の sandbox 内で動きます。foreground timeout 内に終了すれば結果を直接返し、それ以上かかる場合は `job_id` を返します。

最初から background 実行する場合は `start_command` を使います。`poll_job` で完了を確認し、`stop_job` で停止できます。job は session に所属し、最大2時間で終了し、session 終了時にもキャンセルされます。1 session あたり同時に8 jobまで実行できます。

`job_list({session_id, limit?})` は current session が所有する in-memory job の redacted snapshot を返します。返すのは `job_id` と `running` / `completed` / `failed` / `unknown` だけで、running を先頭に並べ、上限超過は `truncated` で示します。command text、argv、stdout/stderr、raw error は返さず、list しても completed result は消費しません。`retention="in_memory"` なので、空のlistを「過去に何も実行していない証拠」とは扱わないでください。restartやcache expiry以前の履歴ではありません。

stdout/stderr の保持量は合計 1 MiB までで、超過時は truncated として返します。

### Experimental Codex task

opt-in の `codex_status`、`codex_task_start`、`codex_task_get`、`codex_task_control` は、local `codex app-server --stdio` に接続し、名前付きの status/task 操作だけを扱います。app-server の handshake は `0.153.4` として検証します。task は完全な session instance と canonical working directory に所有されるため、別 session、別 process generation、別 scope から resume できません。

`codex_task_start` と `codex_task_control` には opaque な `operation_id` が必須です。control action は型付きの `steer` / `resume` / `interrupt` だけです。Temote は child turn の start/control より先に accepted receipt を永続化します。crash で副作用の成否が不明な場合は盲目的に replay せず `reconciliation_required` を返します。通常 session では local approval が必要です。approval detail には Codex provenance、operation/tool、target と scope、mutation/read-only、safe な model/effort または command/file-change summary を表示します。prompt、control input、transcript、raw command argument、patch body、command output は task metadata や approval/activity summary に保存しません。`codex_task_get` の詳細 thread data は、opaque な `evidence_id` で取得する bounded・期限付き・session/scope限定の evidence だけです。

Temote の yolo は Temote 自身の local sandbox と approval behavior だけを変更し、Codex child の mutation を認可しません。Codex app-server の command/file-change approval request は child 側の approval boundary を維持し、user approval transport が利用できない場合は fail closed します。thread 作成前の initialization または model/list の失敗は `retryable_failed` として同じ start operation を再試行できます。一方、thread/start または turn/start の request 送信後に成否が不明になった場合は replay せず `reconciliation_required` を維持します。`codex_task_get` は `after_revision` / `not_modified` を判定する前に remote thread を reconcile します。task record は task retention の全期間、unexpired の terminal record を含めて保持され、expired かつ live child runtime のない terminal record だけが prune 対象です。scope が retention limit に達した場合は、unexpired record を削除せず新しい start を拒否します。compact された operation receipt も retention 中の exact replay/conflict 検出を維持します。

生成される turn には Codex の `workspaceWrite`、session の canonical directory を writable root、network disabled を指定します。これは Temote の直接 `execute` sandbox と同じ OS-level boundary ではなく、experimental な app-server adapter です。app-server process 自体は inference service と直接通信するため、Codex build と sandbox behavior を検証できない場合はこの surface を無効または opt-in のままにしてください。generic JSON-RPC、remote shell、automatic approval は公開しません。

### 構造化ローカルエージェント broker

`local_agent_run({session_id, agent, task, cwd?, access, model?, profile?})` は、インストール済みの Codex または OpenCode を構造化された broker 経由で1回実行します。
`agent` は `codex` と `opencode` に限定し、呼び出し側が渡せるのは bounded な task と access mode だけです。
実行ファイル、raw argv、environment、network policy は Temote が構築し、caller から指定できません。
実装対象の non-interactive CLI contract は、インストール済み CLI の help 出力で検証します。

指定した `cwd` は canonicalize し、symlink 解決後も yolo を含むすべての session で permitted root 内に限定します。
permitted root は選択可能な `cwd` の範囲を認可するものであり、child に自動公開する path の一覧ではありません。
Temote 側の agent profile では、選択した canonical `cwd` だけを agent workspace として再公開します。
`workspace_write` では選択 cwd だけを書き込み可能にし、`read_only` では選択 cwd を読み取り専用にします。
他の permitted root は agent に自動公開しません。
どちらの mode でも agent の state と cache は毎回専用 directory に分離して書き込み可能にし、選択 workspace 以下のすべての `.git`、`.agents`、`.codex`（nested を含む）は保護したままにします。

local agent の request は yolo session からでも local approval boundary を通ります。
deny の場合は child process を起動せずに終了します。
Codex の task は 1 MiB まで受け付け、検証済みの `codex exec ... -` contract に従って stdin で渡すため argv には載せません。
インストール済み OpenCode の `run [message..]` には検証済みの stdin prompt transport がないため、positional message の task は 64 KiB に制限します。
child の stdout/stderr 合計は 1 MiB に制限します。
foreground timeout を超える場合は通常の Temote `job_id` を返し、`poll_job` または `stop_job` で確認または停止できます。
interactive approval detail には control character を sanitize した bounded な task preview を表示します。
永続化する activity / metadata には agent、scope、access mode、task byte数、SHA-256 だけを記録し、task 本文・preview・environment value は記録しません。

child environment は一度消去して最小限の allow-list から再構成するため、Temote が保持する credential、token、proxy 設定を暗黙には渡しません。
外側の agent profile は host の temporary root と user-agent state root を隠し、今回必要な workspace、実行ファイル directory、private run state だけを再公開します。
既存の Codex (`~/.codex/auth.json`) と OpenCode (`~/.local/share/opencode/auth.json`) の login file は、top-level agent runtime が使える bounded な read-only input として private な run state に取り込みます。
broker が Codex の strict permission profile と OpenCode の read / external-directory restriction を固定するため、model-generated command/tool execution から imported auth file は読めません。
元の user file は hidden にし、child から書き込みできません。
agent の file edit は Git remote 操作の認可を与えません。
stage、commit、fetch、pull、push には専用の `git_*` tool を使います。
公開 HTTP はこの構造化 broker だけを公開し、generic な `without_sandbox` tool は引き続き公開しません。

## work checkpoint / handoff

`checkpoint_save` は client が申告した bounded checkpoint を Temote の private state に保存し、session の current canonical working directory をscopeにします。すべてのsaveで opaque UUID `operation_id` が必須です。新規作成は `checkpoint_id` を省略して `expected_revision=0`、更新は既存UUIDとcurrent revisionを指定します。同じlogical mutationを同一`operation_id`かつ同一canonical requestで再送すると、checkpoint/revisionを増やさず以前のcommit結果を返します。同じoperation IDを異なるrequestで再利用すると `OPERATION_CONFLICT`、通常のstale revisionは `CHECKPOINT_CONFLICT` になります。operation receiptはcheckpointと同じatomic write境界で永続化し、bounded historyとして保持します。通常sessionではlocal approvalが必要で、yoloは既存のauto-approval semanticsを維持します。`checkpoint_load` は同じcanonical cwd scopeからだけ読め、同じworktreeなら別sessionからも読めます。

checkpointのstatus/check resultは常に `source="client_reported"` です。`verified` もreported checkとcommitの整合性を検査するだけで、command成功からTemoteが自動的にverificationを認定することはありません。title/descriptionへcredential、token、private command outputなどのsecretを書かないでください。approval/activity summaryにはtoolとstep/check件数だけを出し、自由文のcheckpoint本文は転載しません。

`work_handoff({session_id, checkpoint_id?})` はread-onlyです。ID省略時は同scopeのbounded checkpoint候補を自動選択せず一覧化します。ID指定時はそのcheckpoint、current-session jobのredactedな `source="live_snapshot"`、`freshness="not_revalidated"`、resume hintに加えて、checkpoint titleとclient-reported next-step descriptionからローカルで構築したbest-effortの `automatic_recall` を返します。automatic recallはrepo-managedな `learnings/` indexだけを使い、network不要で、hitがある場合だけ `review_recalled_learnings` を追加します。checkpoint本文をcommandとして実行せず、作業のreplay、Git実行、artifact検証もしません。

安全なresume flowは `session_info` → `work_handoff` → checkpointを選択 → `work_handoff(checkpoint_id=...)` → running jobを再実行前にinspect/poll → Git state・artifact・checkを別のread-only手段で再検証 → 次のoperationを決める、です。

## bounded multi-file patch

`apply_patch({session_id, patch})` は Codex-style の `*** Begin Patch` 形式で add / update / move / delete を扱います。patch本文をshellで実行せず、Rust側で直接parseします。最初のwriteより前に全source/destinationを検査し、absolute/traversal pathとsymlink escapeを拒否し、patch/file sizeとoperation数をboundedにし、yoloでもsession root外へ出さない契約です。通常sessionはpreflight済みpatch全体に対してlocal approvalを1回だけ要求します。approval/activity metadataへpatch本文は保存せず、operation件数だけを出します。

multi-file全体をtransactional atomicとは扱いません。途中I/O errorが発生した場合は `partial_failure` と machine-readableな `committed` listを返し、errorまでに完了したadd/update/move/deleteを正確に示します。malformed patchまたはpreflight failure時は1 fileもwriteしません。

## friction / learning candidate / recall

Temoteはowner-onlyかつboundedなfriction event storeへ、event/session ID、canonical scope、enum kind/source/outcome、restrictedなoperation/tool identifier、optional UUID linkだけを保存します。command argv、stdout/stderr、file content、prompt、approval本文、environment value、credential、transcriptは保存しません。現在のautomatic emitterはcommand/Git failure、`apply_patch` のambiguous partial mutation、normal sessionで実際に返されたnegative approval responseです。runtime shutdownでpending promptが閉じただけのケースはuser denialとして誤記録しません。`recall_feedback(outcome="no_hit")` はqueryやrecall resultを保存せず、明示的な `client_reported` knowledge-gap signalだけを追加できます。

`friction_summary({session_id})` はread-onlyで、kind別countとcap済みcontributionを含むexplainable scoreを返します。正常sessionはtool call数が多いだけではcandidateにならず、同kindのrepeated failureはbounded、recall miss単独はscore 0です。`learning_candidate_list({session_id})` はsummaryからreview用candidateをderived viewとして返しますが、authoritative learningへ自動publishせず、checkpoint本文、transcript、command outputもcandidateへコピーしません。

authoritative learningはrepo-managed Markdownです。`recall({session_id, query, knowledge_root?, limit?})` はrelative `learnings/` をdefault rootとし、明示した別rootもsession root内だけ許可します。毎requestでdeterministic local indexを再構築し、network、embedding API、vector DBは不要です。各Markdown learningには `title`、`date` (`YYYY-MM-DD`)、1個以上のbounded `tags`、`domain`、`verification` と、`## Problem` / `## Resolution` / `## Reusable lesson` sectionが必要です。recall resultはhitごとのmatched/missing termとscoreを返します。learningのpublish/editは既存の `write_file` + Git trust/approval flowを使います。

## file / image

- `list_directory`: directory 一覧
- `read_file`: UTF-8 text 読み込み
- `get_image`: 対応 image を MCP image content として取得
- `write_file`: 選択中の permission mode に従って UTF-8 text を書き込み

## Git

通常の sandbox command から Git metadata は書き換えられません。Git 変更には専用 tool を使います。

- `git_add`: 明示した path を stage
- `git_commit`: hooks/signing 無効で index を commit
- `git_fetch`: 設定済み remote を fetch
- `git_pull`: fast-forward-only
- `git_push`: current branch を push。force や任意 URL/refspec は受け付けない

remote Git 操作は host operation なので、通常 session ではローカル承認が必要です。

## Yolo mode

```sh
temote-mcp start my-project --yolo
```

Yolo mode では Temote MCP の path 制限、command sandbox、ローカル承認を意図的に外します。Temote MCP を実行しているユーザーの filesystem、environment、process、network 権限で動作します。MCP client や外部システム側の authorization/confirmation まで無効にするものではありません。

detached supervisor は、実行中の通常 session を暗黙に yolo へ昇格させません。yolo が必要な場合は、意図した trust level として local-only compatibility command から明示的に起動します。

## local stdio

MCP client が Temote MCP process を直接起動する場合:

```sh
temote-mcp mcp
```

local stdio では、ローカル承認付きの `without_sandbox` を公開できます。公開 HTTP endpoint では公開しません。

## 安全上の注意

- project directory で足りる場合に home directory 全体のような広い root を許可しないでください。
- secret-file denylist はありません。filesystem の主な境界は permitted root です。
- runtime audit は operation/status/timing を記録し、command 引数、output、認証 identity、secret value は永続化しません。
- secret を使う integration は credential を session process に保持し、session metadata へ保存しません。
