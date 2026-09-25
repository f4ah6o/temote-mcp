# Temote MCP の使い方

[English](usage.md)

## session

local work では session を直接作成します。lifecycle supervisor が未起動なら、local CLI が現在の Temote binary 自身を supervisor として起動し、control socket が ready になるまで待ちます。新規 session は sandbox を維持した approval-free の `agent` mode が既定です。

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp session start my-project --path src/my-project
temote-mcp session list
temote-mcp session info my-project
```

local approval input が必要な場合は `temote-mcp session console` を使います。この console を閉じる、または stdin EOF になっても runtime は停止せず、console だけが detach します。console 不在中の approval-required operation は fail closed します。

supervisor の bounded local activity stream は `temote-mcp activity [SESSION_ID] [--tail N] [--no-follow]` で確認できます。既定では follow し、filter 後の最新100件を replay します。durable audit log ではなく best-effort な診断機能です。privacy、欠落、retention、切断時の動作は [managed session と named root](managed-sessions.ja.md#local-activity-viewer) を参照してください。

installed binary の更新後は `temote-mcp upgrade --dry-run` → `temote-mcp upgrade` で compatible な same-PID supervisor handoff と coordinated session restart/restore を行えます。credential value は永続化せず、restart context 不足または in-flight operation があれば中止し、planned session を全て確認してから成功を返します。handoff protocol 導入前の supervisor からは最初に手動 restart が1回必要です。

`session list` では durable な `starting` / `active` / `stopping` / `stopped` / `crashed` に加え、durable metadata は残っているが canonical な working directory または permitted workspace root が解決できなくなった session を `degraded` として確認できます。degraded entry は保存済みの ID、path、lifecycle timestamp を保持したまま返し、listing 全体を失敗させません。消失した path を stopped / crashed runtime と読み替えることはなく、`session info` も同じ bounded な degraded view を返します。`session info` では working directory、permitted root、permission mode、timestamp、exit reason、last error を確認できます。working directory が対応する標準 Git worktree 配下にある場合は、configured な `src` named root から導出した非 secret の `workspace` identity(`canonical_checkout` / `managed_worktree` / `legacy_worktree` の `workspace_type` と、判明していれば `repository_root`、`workspace_root`、`repository`、`branch`、managed task 名)も返します。identity は read-only で、workspace が解決できなくなると表示されません。死んでいる、または liveness が曖昧な socket を暗黙に active とは扱いません。manual restart は `temote-mcp session restart <id>` で行えます。自動 restart は現時点では有効化しません。restart は old full session instance を fence し、replacement の開始前に登録済み Codex runtime を shutdown します。replacement の開始に失敗しても old child runtime は残しません。

session discovery は active-first です。running supervisor が所有する session を bounded な historical metadata より先に返すため、履歴が蓄積しても active session が `session list` / MCP `session_list` から押し出されません。historical な stopped / crashed entry は list budget 内で deterministic な recent-first 順に返しますが、workspace が解決できなくなった historical metadata は bounded history から除外します（metadata は保持し、`session info` は引き続き degraded view を返します）。supervisor startup と periodic maintenance では、安全に terminal と確認できた metadata pair のうち最近512件を保持し、それより古い confirmed stopped / crashed pair だけを prune します。live、曖昧、malformed / orphan、supervisor upgrade restore plan で保護されている metadata は retention で自動削除しません。read-only listing と MCP fallback は cleanup を行いません。

`temote-mcp session forget <id>` は、terminal で non-live な1 session の Temote-owned durable state（metadata、lifecycle state、stale と確認済みの socket entry）を削除します。`stop` は後から `session list` / `session info` で参照できるよう metadata を保持し、`forget` は意図的に削除します。runtime socket probe が live を返した場合は無条件で拒否し、supervisor の lifecycle transition と直列化され、symlink や非 regular file の metadata target を拒否し、workspace、cwd、worktree には触れません。1 session の forget は他 session の retention policy を変更しません。

`temote-mcp session gc` は滞留した orphan metadata 半身のための bounded maintenance path です。既定は dry-run plan（`--apply` で削除）で、`--limit 1..=1000`（既定 100）を受け付けます。初期レビュー済み class だけが対象です: 24時間の grace period より古い、regular で symlink ではない、supervisor-owned でも upgrade-protected でもなく、live session socket probe が応答しない、lone `.state` lifecycle 半身（`missing_json`）または lone `.json` metadata 半身（`missing_state`）。後者の metadata は session ID が一致して読める必要があります。malformed、ID mismatch、symlink、special file、grace 内の entry は報告のみで削除しません。plan は deterministic な bounded limit のため oldest-first で並び、`--apply` は削除直前に各 candidate を再検証し、drift や concurrent start があった entry は skip します。cleanup は Temote-owned session metadata file に限定され、workspace、cwd、worktree には触れません。

互換用に `cd ~/src/my-project && temote-mcp start my-project` も利用できます。current directory を起動し、必要なら同じ local supervisor も自動起動します。`temote-mcp start my-project --yolo` は意図的に制限を外す local-only form として残します。

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

named root は Temote MCP 起動前に host 側の `TEMOTE_MCP_ROOTS` で設定します。`TEMOTE_MCP_ROOTS='src=~/src'` のような単一 mapping、または `TEMOTE_MCP_ROOTS='{"src":"~/src","opt":"~/opt"}'` のような JSON object を設定してから Temote MCP を再起動してください。未設定の場合 `session_start` は無効のままで、named-root resolution の error に設定方法が表示されます。実行中 session の root は後述の host-approved `directories` grant で restart なしに追加することもできます。

従来の inline `/permission ...` terminal command UI は detached runtime の owner ではなくなったため、第一段階の supervisor control surface には載せていません。権限を広げる変更ではなく、runtime は persisted permitted root のまま fail closed します。

## session capability grant

実行中の sandboxed session は、local `temote-mcp session permission <id> grant|ungrant` 経由の host approval で個別スコープの capability grant を session metadata に永続化できます。承認された grant は session restart 後も維持され、`session_info` の `grants` に表示されます。permitted directory の削除は従来どおり `session permission revoke <path>` を使います。

- `directories`（最大16、absolute path のみ）は session の permitted root を実行中に拡張します。起動 directory の原則は変わりません。最初から広い root で起動するのではなく、host approval で必要な directory を追加してください。
- `ambient_git_credentials` を grant すると、delegated agent 内の managed Git 操作は repository に managed GitHub credential mapping がない場合に限り ambient な host Git credential（credential helper、forward された `ssh-agent`）に fallback します。managed mapping が設定されている場合はそちらが優先され、global な `gh` auth state は一切変更しません。

## delegation と job

Temote は file、command、Git、integration を直接実行しません。machine 上の作業は下記の task backendを通じて local 側の coding agent に委譲します。task の transcript や child output は bounded で期限付きの session/scope 限定 evidence としてだけ境界を越え、`evidence_read({session_id, evidence_id, offset_bytes?, max_bytes?})` で読みます。

foreground timeout を超える作業は session 所有の `job_id` を返します。完了が必要なら `poll_job` で確認し、不要になったら `stop_job` で停止します。job は session に所属し、最大2時間で終了し、session 終了時にもキャンセルされます。

`job_list({session_id, limit?})` は current session が所有する in-memory job の redacted snapshot を返します。返すのは `job_id` と `running` / `completed` / `failed` / `unknown` だけで、running を先頭に並べ、上限超過は `truncated` で示します。command text、argv、stdout/stderr、raw error は返さず、list しても completed result は消費しません。`retention="in_memory"` なので、空のlistを「過去に何も実行していない証拠」とは扱わないでください。

stdout/stderr の保持量は合計 1 MiB までで、超過時は truncated として返します。

delegation backend が返す task view は、backend の execution `status` を維持したまま、3 つの状態を分けて持ちます。`execution` は論理 task の現在の execution の安定した id と generation、`verification` は `not_run` / `passed` / `failed` と、その結果を記録した task record revision への束縛、`delivery` は `not_started` / `pending` / `submitted` / `merged` / `closed` / `failed` です。execution が `completed` でも verification PASS にはなりません。現在の revision に適用できる結果がない間は `verification.status` は `not_run` のままで、古い revision の結果は現在の PASS としてではなく `stale: true` と以前の target 付きで返します。delivery は記録された delivery 操作だけが更新し、agent が報告した pull request では変わりません。これらの field より前に書かれた record は `not_run` / `not_started` として読めます。

### Experimental Codex task

opt-in の `codex_status`、`codex_task_start`、`codex_task_get`、`codex_task_control` は、local `codex app-server --stdio` に接続し、名前付きの status/task 操作だけを扱います。互換性を Codex app-server の特定 version 文字列には固定しません。initialize response は上限付きで shape を検証し、version は解析できた場合だけ best-effort の診断情報として返します。互換性は、Temote が実際に使用する `model/list`、`thread/*`、`turn/*` の request/response をその場で検証して fail-closed にします。task は完全な session instance と canonical working directory に所有されるため、別 session、別 process generation、別 scope から resume できません。

`codex_task_start` と `codex_task_control` には opaque な `operation_id` が必須です。control action は型付きの `steer` / `resume` / `interrupt` だけです。型付き `resume` は保持済みの thread / turn を reconcile する操作であり、新しい turn の開始や終了済み child process の再起動は行いません。Temote は child turn の start/control より先に accepted receipt を永続化します。crash で副作用の成否が不明な場合は盲目的に replay せず `reconciliation_required` を返します。`ask` mode ではこれらの操作に local approval が必要で、`agent` と `yolo` session では Temote-local prompt を skip します。approval detail には Codex provenance、operation/tool、target と scope、mutation/read-only、safe な model/effort または command/file-change summary を表示します。prompt、control input、transcript、raw command argument、patch body、command output は task metadata や approval/activity summary に保存しません。`codex_task_get` の詳細 thread data は、opaque な `evidence_id` で取得する bounded・期限付き・session/scope限定の evidence だけです。

Temote の yolo は Temote 自身の local sandbox と approval behavior だけを変更し、Codex child の mutation を認可しません。Codex app-server の command/file-change approval request は child 側の approval boundary を維持し、user approval transport が利用できない場合は fail closed します。thread 作成前の initialization または model/list の失敗は `retryable_failed` として同じ start operation を再試行できます。一方、thread/start または turn/start の request 送信後に成否が不明になった場合は replay せず `reconciliation_required` を維持します。`codex_task_get` は `after_revision` / `not_modified` を判定する前に remote thread を reconcile します。別の Temote process が live app-server runtime を所有している場合は、競合する resume を起動せず、永続化済み revision と `reconciliation_deferred: true` を返します。typed control は operation receipt の永続化前に失敗します。その lease が live の間、session cleanup は task record を上書きせず、runtime owner が session 終了を検知して child を停止し task を finalize します。task record は task retention の全期間、unexpired の terminal record を含めて保持され、expired かつ live child runtime のない terminal record だけが prune 対象です。scope が retention limit に達した場合は、unexpired record を削除せず新しい start を拒否します。compact された operation receipt も retention 中の exact replay/conflict 検出を維持します。

生成される turn には Codex の `workspaceWrite`、session の canonical directory を writable root、network disabled を指定します。これは Temote の session sandbox と同じ OS-level boundary ではなく、experimental な app-server adapter です。app-server process 自体は inference service と直接通信するため、Codex build と sandbox behavior を検証できない場合はこの surface を無効または opt-in のままにしてください。generic JSON-RPC、remote shell、automatic approval は公開しません。

### Experimental OpenCode task

opt-in の `opencode_status`、`opencode_task_start`、`opencode_task_get`、`opencode_task_control` は、task ごとの `opencode serve` child を loopback 上に起動し、`unofficial-opencode-sdk` HTTP client 経由で操作します。各 serve child は 127.0.0.1 の動的 port、child 環境変数経由のみで渡す instance ごとの random Basic-auth password、task ごとの隔離 data directory、`OPENCODE_CONFIG_CONTENT` で注入される上限付き serve permission 設定で動きます。host の OpenCode global 設定（provider/model 定義を含む）を読み、従来の `auth.json` と OpenCode V2 の SQLite 保存資格情報を task 専用 state に取り込みます。host の session や履歴は取り込みません。先に host の CLI で `opencode auth login` を行ってください。資格情報は spawn 時点の copy であり、child による token 更新は host のアカウントへ戻りません。未対応の V2 credential schema は安全側に失敗します。task record、ownership、lease、receipt、retention、scoped evidence は上記 Codex app-server task と同じ契約です。task は完全な session instance と canonical working directory に所有され、別 session、別 process generation、別 scope から resume できません。

`opencode_task_start` と `opencode_task_control` には opaque な `operation_id` が必須です。control action は型付きの `steer` / `resume` / `interrupt` だけです。steer は保持済み session に追加の prompt を送り、resume は保持済み session を reconcile した上で同じ task 用 state directory を使う新しい serve child に spawn し直します (新しい task の開始や終了済み child の再起動ではありません)。prompt には operation 由来の deterministic `messageID` が付くため、`opencode_task_get` は start prompt が server に受理されたかを判定してから `reconciliation_required` を決めます。terminal state の report は 他の delegation backend と同じ bounded report contract の下、最後の assistant message から抽出します。usage と observed model は self-reported 値ではなく session message から読みます。`ask` mode では Codex task と同じ provenance/scope/mutation metadata を伴う local approval が必要で、`agent` と `yolo` session では Temote-local prompt を skip します。`opencode_task_get` の詳細は bounded・期限付き・session/scope限定の evidence だけです。

これは Temote の session sandbox と同じ OS-level boundary ではなく、experimental な serve adapter です。serve process 自体は provider API と直接通信します。保留中の OpenCode permission/question request は auto-approval ではなく `waiting_approval` task state として表面化します。host の OpenCode build を end-to-end で検証するまでは、この surface を opt-in のままにしてください。

OpenCode 1.x と 2.x では HTTP contract が異なります (`global/*` と `api/*`)。起動時に各 poll で `global/health` を先に probe し、v1 probe が失敗した poll から `api/*` の liveness route (`api/info`、次に `api/health`) も試行して、先に応答した contract を採用します。両 adapter は同じ task interface を提供するため、両 contract を提供する build ではどちらが選ばれても正しく動作します。`TEMOTE_OPENCODE_SERVE_CONTRACT` に `v1` または `v2` を設定すると auto-detection を skip して contract を固定できます。それ以外の値または未設定では auto-detection のままです。

### Experimental Devin task

opt-in の `devin_status`、`devin_task_start`、`devin_task_get`、`devin_task_control` は、task ごとの `devin acp` child を起動し、stdio 上の JSON-RPC で Agent Client Protocol (ACP) を話します。child は task の canonical scope directory で、filter 済み環境変数 (PATH/HOME/proxy と Devin/Windsurf の credential 変数のみ) 付きで動きます。task record、ownership、lease、receipt、retention、scoped evidence は上記 Codex / OpenCode task と同じ契約です。task は完全な session instance と canonical working directory に所有され、別 session、別 process generation、別 scope から resume できません。

`devin_task_start` と `devin_task_control` には opaque な `operation_id` が必須です。control action は型付きの `steer` / `resume` / `interrupt` だけです。steer は保持済み ACP session に追加の `session/prompt` を送り、resume は agent が `loadSession` capability を advertise する場合に限り `session/load` で reattach します (advertise が無ければ replay せず fail closed)、interrupt は `session/cancel` を送ります。各 `session/prompt` は blocking の ACP request で、その `stopReason` result が task status に対応します (`end_turn` は completed、`cancelled` は interrupted、それ以外は `retryable_failed`)。送信済みの可能性がある turn の応答を失った場合は replay せず保持済み session state から reconcile します。agent 側の `session/request_permission` は auto-approval ではなく Temote-local approval console 経由の `waiting_approval` task state として表面化します。terminal report は他の delegation backend と同じ bounded report contract の下、蓄積した assistant message から抽出します。`ask` mode では同じ Devin provenance/scope/mutation metadata を伴う local approval が必要で、`agent` と `yolo` session では Temote-local prompt を skip します。`devin_task_get` の詳細は bounded・期限付き・session/scope限定の evidence だけです。

これは Temote の session sandbox と同じ OS-level boundary ではなく、experimental な stdio adapter です。ACP child は Devin service と直接通信し、認証は install 済み CLI 自身の credential state (`devin auth login`、`DEVIN_API_KEY`、または `WINDSURF_API_KEY`) を使います。host の Devin CLI build を end-to-end で検証するまでは、この surface を opt-in のままにしてください。

`devin_task_start` は `cloud: true` も受け付け、その場合は `devin acp --cloud` を spawn します。CLI が stdio ACP transport を Devin Cloud の ACP WebSocket に relay するため、local agent ではなく CLI の `auth login` account 上で hosted session が動きます。`model` と `agent` は `devin acp --cloud` では無視されるため、`cloud` と併用すると拒否されます。transport、ownership、evidence の契約は local mode と同じです。

### Experimental Devin Cloud task

上の `devin acp` tool は *local* の Devin CLI を駆動します。これとは別の `devin_cloud_status`、`devin_cloud_task_start`、`devin_cloud_task_get`、`devin_cloud_task_control` は、Temote host から HTTPS で Devin API v3 (`https://api.devin.ai/v3/organizations/{org_id}/sessions`) を呼び、*hosted* な Devin session を駆動します。Devin binary は不要で、作業は Temote session の working directory ではなく Devin Cloud 側で行われます。これらの tool は `network` feature build にのみ存在します。

credential は `TEMOTE_MCP_DEVIN_API_KEY` (Devin service-user key または personal API key。fallback として `DEVIN_API_KEY` も受け付けます) で設定します。`TEMOTE_MCP_DEVIN_ORG_ID` で organization を固定でき、未設定なら `/v3/self` から一度だけ解決します (曖昧な場合は fail closed)。`https://` の `TEMOTE_MCP_DEVIN_API_BASE_URL` で API origin を上書きできます。service user と personal access token はどちらも通常の (Teams / self-serve) subscription の organization settings で発行でき、session はその subscription の ACU を消費します。service-user key を使う場合は `TEMOTE_MCP_DEVIN_CREATE_AS_USER_ID` に自分の Devin user ID を設定すると、session が service user ではなく自分に帰属します (`create_as_user_id`。service user の role が許可している必要があります)。`devin_cloud_status` と `temote-mcp doctor` が報告するのは credential の *source* (変数名)、organization、base URL だけで、key の値は task record、approval、evidence、tool output のどこにも現れません。

`devin_cloud_task_start` には opaque な `operation_id` と `task` が必須で、`title`、`devin_mode`、`repos`、`max_acu_limit` は任意で session 作成に転送されます。Temote は API 呼び出し前に acceptance を永続化し、`temote-mcp` tag 付きの resumable session を1つ作成します。prompt では他の delegation backend と同じ bounded JSON report contract を要求し、structured output としても要求します。task record は Codex / OpenCode / Devin ACP task と同様に完全な session instance と canonical working directory に所有され、`devin-cloud-tasks` 配下に保存され、他 session からは見えません。`devin_cloud_task_get` は保持済み task を hosted session の status と reconcile します (`running`/`claimed` → `running`、`waiting_for_user` → `waiting_input` (ただし terminal report が公開済みなら `completed`/`failed` — Devin は turn 終了後に exit せず idle するため)、`waiting_for_approval` → `waiting_approval`、inactivity による `suspended` → `waiting_input`、`exit` → `completed`、`error`/quota/payment failure → `failed`、user による terminate → `interrupted`)。report は structured output を優先し、無ければ最後の Devin message から抽出します。最後の message は bounded な scoped evidence 経由でのみ公開します。control action は `steer` (follow-up message を送信)、`resume` (suspended session に message を送り Devin 側で resume)、`interrupt` (hosted session を terminate) です。API の明確な reject は `retryable_failed`、remote 効果が不確定な transport failure は `reconciliation_required` になり、blind replay は行いません。`ask` mode では Devin Cloud provenance metadata (`scope: devin_cloud`) 付きの local approval が必要で、その mutation が local workspace ではなく hosted organization の ACU を消費することを operator が確認できます。

### Delegation backend (ローカル CLI)

`temote-mcp delegate --backend codex|opencode ...` は、ローカル CLI から bounded な非対話 delegation を1回実行し、bounded な JSON result を1つ出力します。OpenCode は明示的な `--session <id>` resume も受け付けます。resume 前に bounded な read-only `opencode session list --format json` preflight を行い、session の canonical directory が現在の canonical delegation directory と一致する場合だけ `run` を起動します。metadata の欠落、曖昧さ、不正 JSON、サイズ超過、probe失敗、directory不一致は fail closed で、`run` より前に拒否します。`--fork` を使うには `--session` が必要で、指定した session の context を継承した新しい session を開始します。`run` の前に、同じ fail closed の directory preflight を親 session に対して実行します。`--continue` と `--attach` はサポートしません。OpenCode backend の executable は次の順で解決します。

1. `TEMOTE_OPENCODE_BIN` が設定されている場合、既存の実行可能な regular file への絶対 path でなければなりません。symlink は canonical target に解決します。PATH より優先されます。
2. 未設定の場合は PATH から `opencode` を解決します。

明示された `TEMOTE_OPENCODE_BIN` が不正（空、相対 path、存在しない、regular file でない、実行可能でない）な場合は fail closed とし、PATH 上の別 executable へ暗黙に fallback しません。設定された path は diagnostics や error に出力しません。`temote-mcp delegate diagnose --backend opencode` が示すのは `available` / `unavailable`、source (`env_override` / `path` / `invalid_override`)、invalid override の bounded な reason だけです。`TEMOTE_OPENCODE_BIN` は parent process が読むだけで、OpenCode child の environment には渡しません。

## delegated session 内の Git

Git metadata への書き込みや remote 同期は Temote tool ではなく委譲された agent 内で実行されます。delegated agent が GitHub HTTPS remote を操作する場合、repository-local の managed credential mapping を host 側で clone ごとに1回設定してください。

```sh
git config --local credential.helper ''
git config --local --add credential.helper '!gh git credential --managed'
git config --local credential.useHttpPath true
```

opt-in として、host 承認済みの `ambient_git_credentials` session grant を使うと、mapping がない場合に限り同じ managed Git 操作が ambient Git credential に fallback します。GitHub 以外や SSH remote は従来どおりの credential 経路を使い、global な `gh` auth state は一切変更しません。

## Yolo mode

```sh
temote-mcp start my-project --yolo
```

Yolo mode では Temote MCP の path 制限、command sandbox、ローカル承認を意図的に外します。Temote MCP を実行しているユーザーの filesystem、environment、process、network 権限で動作します。MCP client や外部システム側の authorization/confirmation まで無効にするものではありません。

detached supervisor は、実行中の通常 session を暗黙に yolo へ昇格させません。yolo が必要な場合は、意図した trust level として local-only compatibility command から明示的に起動します。

## local stdio

Temote MCP を直接起動する MCP client 向け:

```sh
temote-mcp mcp
```

## 安全上の注意

- project path で足りる場合に home directory 全体のような広い root を許可しないでください。
- secret-file の denylist はありません。permitted root が session scope の主要な filesystem 境界です。
- runtime audit は operation/status/timing metadata だけを記録し、task 本文、child output、認証 identity、secret 値は記録しません。
- delegated backend は credential を session metadata ではなく child/session process 内に保持します。

## リモートアップグレードと再接続

認証済みの直接 HTTP では `upgrade_preflight`、`upgrade_apply`、
`upgrade_status` を公開します。これらのツールは stdio MCP とマルチホスト
ゲートウェイには公開しません。preflight と status は読み取り専用のホスト
ライフサイクル操作です。apply が受け付けるのは、実行中で管理対象の通常
セッションを示す `session_id` と、省略可能な `expected_version` だけです。
実行ファイルのパス、URL、コマンド、argv、環境変数は指定できません。
ask と agent のどちらでもローカル利用者の明示的な承認が必要で、公開 yolo
セッションは拒否されます。

`upgrade_apply` が新しいトランザクションを accepted として返した後、Temote
はその HTTP 接続を閉じ、独立したローカル coordinator が残りの処理を所有します。
クライアントは通常の認証で同じエンドポイントへ再接続し、initialize または
ping のホスト、バージョン、boot identity を確認してから、
`upgrade_status(transaction_id)` が終端状態になるまで確認してください。
Temote は任意の MCP クライアントに再接続を強制できません。成功状態は、対象
バージョン、安定したホスト identity、セッション復元、および ingress を交換した
場合の新しい boot generation を coordinator が検証したことを示します。
