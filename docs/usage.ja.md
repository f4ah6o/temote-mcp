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

実行中の sandboxed session は `session_permission_request({session_id, listen_ports?, dev_tool_env_prefixes?, ambient_git_credentials?, directories?})` で個別スコープの追加 capability を request できます。各 field は additive で、empty でない field はすべて local approval console を通ります。host は承認前に正確な port、prefix、path を確認します。field を束ねると1つの approval prompt にまとまるため、automation が必要な capability を一度の host 操作で集められます。承認された grant は session metadata に永続化され、session restart 後も維持され、`session_info` の `grants` に表示されます。local CLI では `temote-mcp session permission <id> grant|ungrant` と対応する option が同等の操作です。permitted directory の削除は従来どおり `session permission revoke <path>` を使います。

- `listen_ports: [5173, ...]`（最大64）を grant すると、`execute` / `start_command` の `allow_loopback_listen: true` を併用した call に限り、その port の TCP listener bind を許可します。macOS の sandbox は bind を loopback に限定できないため、granted port はすべての interface で bind 可能です。workload が必要とする port だけを grant してください。Linux の development profile はもともと listen を許可するため、この option は no-op です。`port_check({session_id, port})` は host から `127.0.0.1:<port>` に接続して accept されるかを報告します。probe できるのは granted port のみで、workspace の観測 tool として機能し、汎用 port scanner にはなりません。
- `dev_tool_env_prefixes: ["MADOBE_", "CARGO_"]`（最大32、各 prefix は `[A-Za-z0-9_]` のみの 1〜64 byte）を grant すると、`dev_tool_run` が granted prefix で始まる名前の `env` object を受け付けます。name/value は bounded で NUL を含めず、`npm_config_ignore_scripts` など broker が設定する変数は上書きできません。
- `ambient_git_credentials: true` を grant すると、validated `git_fetch` / `git_pull` / `git_push` / `git_push_tag` は repository に managed GitHub credential mapping がない場合に限り ambient な host Git credential（credential helper、forward された `ssh-agent`）に fallback します。managed mapping が設定されている場合はそちらが優先され、global な `gh` auth state は一切変更しません。
- `directories: ["/abs/path", ...]`（最大16、absolute path のみ）は session の permitted root を実行中に拡張します。

起動 directory の原則は変わりません。最初から広い root で起動するのではなく、host approval で必要な directory を追加してください。

## command

`execute` は shell を介さず argv を実行します。`ask` では network 無効の sandbox 内で動き、既定の `agent` では同じ sandbox と path containment を維持したまま localhost / LAN / Internet の開発通信を許可する network-enabled development profile を使います。`yolo` は host 上で制限なく実行する local-only path のままです。foreground timeout 内に終了すれば結果を直接返し、それ以上かかる場合は `job_id` を返します。

最初から background 実行する場合は `start_command` を使います。`poll_job` で完了を確認し、`stop_job` で停止できます。job は session に所属し、最大2時間で終了し、session 終了時にもキャンセルされます。1 session あたり同時に8 jobまで実行できます。

`job_list({session_id, limit?})` は current session が所有する in-memory job の redacted snapshot を返します。返すのは `job_id` と `running` / `completed` / `failed` / `unknown` だけで、running を先頭に並べ、上限超過は `truncated` で示します。command text、argv、stdout/stderr、raw error は返さず、list しても completed result は消費しません。`retention="in_memory"` なので、空のlistを「過去に何も実行していない証拠」とは扱わないでください。restartやcache expiry以前の履歴ではありません。

stdout/stderr の保持量は合計 1 MiB までで、超過時は truncated として返します。

### Experimental Codex task

opt-in の `codex_status`、`codex_task_start`、`codex_task_get`、`codex_task_control` は、local `codex app-server --stdio` に接続し、名前付きの status/task 操作だけを扱います。互換性を Codex app-server の特定 version 文字列には固定しません。initialize response は上限付きで shape を検証し、version は解析できた場合だけ best-effort の診断情報として返します。互換性は、Temote が実際に使用する `model/list`、`thread/*`、`turn/*` の request/response をその場で検証して fail-closed にします。task は完全な session instance と canonical working directory に所有されるため、別 session、別 process generation、別 scope から resume できません。

`codex_task_start` と `codex_task_control` には opaque な `operation_id` が必須です。control action は型付きの `steer` / `resume` / `interrupt` だけです。型付き `resume` は保持済みの thread / turn を reconcile する操作であり、新しい turn の開始や終了済み child process の再起動は行いません。Temote は child turn の start/control より先に accepted receipt を永続化します。crash で副作用の成否が不明な場合は盲目的に replay せず `reconciliation_required` を返します。`ask` mode ではこれらの操作に local approval が必要で、`agent` と `yolo` session では Temote-local prompt を skip します。approval detail には Codex provenance、operation/tool、target と scope、mutation/read-only、safe な model/effort または command/file-change summary を表示します。prompt、control input、transcript、raw command argument、patch body、command output は task metadata や approval/activity summary に保存しません。`codex_task_get` の詳細 thread data は、opaque な `evidence_id` で取得する bounded・期限付き・session/scope限定の evidence だけです。

Temote の yolo は Temote 自身の local sandbox と approval behavior だけを変更し、Codex child の mutation を認可しません。Codex app-server の command/file-change approval request は child 側の approval boundary を維持し、user approval transport が利用できない場合は fail closed します。thread 作成前の initialization または model/list の失敗は `retryable_failed` として同じ start operation を再試行できます。一方、thread/start または turn/start の request 送信後に成否が不明になった場合は replay せず `reconciliation_required` を維持します。`codex_task_get` は `after_revision` / `not_modified` を判定する前に remote thread を reconcile します。別の Temote process が live app-server runtime を所有している場合は、競合する resume を起動せず、永続化済み revision と `reconciliation_deferred: true` を返します。typed control は operation receipt の永続化前に失敗します。その lease が live の間、session cleanup は task record を上書きせず、runtime owner が session 終了を検知して child を停止し task を finalize します。task record は task retention の全期間、unexpired の terminal record を含めて保持され、expired かつ live child runtime のない terminal record だけが prune 対象です。scope が retention limit に達した場合は、unexpired record を削除せず新しい start を拒否します。compact された operation receipt も retention 中の exact replay/conflict 検出を維持します。

生成される turn には Codex の `workspaceWrite`、session の canonical directory を writable root、network disabled を指定します。これは Temote の直接 `execute` sandbox と同じ OS-level boundary ではなく、experimental な app-server adapter です。app-server process 自体は inference service と直接通信するため、Codex build と sandbox behavior を検証できない場合はこの surface を無効または opt-in のままにしてください。generic JSON-RPC、remote shell、automatic approval は公開しません。

### Experimental OpenCode task

opt-in の `opencode_status`、`opencode_task_start`、`opencode_task_get`、`opencode_task_control` は、task ごとの `opencode serve` child を loopback 上に起動し、`unofficial-opencode-sdk` HTTP client 経由で操作します。各 serve child は 127.0.0.1 の動的 port、child 環境変数経由のみで渡す instance ごとの random Basic-auth password、task ごとの隔離 data directory (host の OpenCode auth の private copy を seed)、`OPENCODE_CONFIG_CONTENT` で注入される上限付き serve permission 設定で動きます。task record、ownership、lease、receipt、retention、scoped evidence は上記 Codex app-server task と同じ契約です。task は完全な session instance と canonical working directory に所有され、別 session、別 process generation、別 scope から resume できません。

`opencode_task_start` と `opencode_task_control` には opaque な `operation_id` が必須です。control action は型付きの `steer` / `resume` / `interrupt` だけです。steer は保持済み session に追加の prompt を送り、resume は保持済み session を reconcile した上で同じ task 用 state directory を使う新しい serve child に spawn し直します (新しい task の開始や終了済み child の再起動ではありません)。prompt には operation 由来の deterministic `messageID` が付くため、`opencode_task_get` は start prompt が server に受理されたかを判定してから `reconciliation_required` を決めます。terminal state の report は `local_agent_run` と同じ bounded report contract の下、最後の assistant message から抽出します。usage と observed model は self-reported 値ではなく session message から読みます。`ask` mode では Codex task と同じ provenance/scope/mutation metadata を伴う local approval が必要で、`agent` と `yolo` session では Temote-local prompt を skip します。`opencode_task_get` の詳細は bounded・期限付き・session/scope限定の evidence だけです。

これは Temote の直接 `execute` sandbox と同じ OS-level boundary ではなく、experimental な serve adapter です。serve process 自体は provider API と直接通信します。保留中の OpenCode permission/question request は auto-approval ではなく `waiting_approval` task state として表面化します。host の OpenCode build を end-to-end で検証するまでは、この surface を opt-in のままにしてください。

OpenCode 1.x と 2.x では HTTP contract が異なります (`global/*` と `api/*`)。起動時に各 poll で `global/health` を先に probe し、v1 probe が失敗した poll から `api/health` も試行して、先に応答した contract を採用します。両 adapter は同じ task interface を提供するため、両 contract を提供する build ではどちらが選ばれても正しく動作します。`TEMOTE_OPENCODE_SERVE_CONTRACT` に `v1` または `v2` を設定すると auto-detection を skip して contract を固定できます。それ以外の値または未設定では auto-detection のままです。

### Experimental Devin task

opt-in の `devin_status`、`devin_task_start`、`devin_task_get`、`devin_task_control` は、task ごとの `devin acp` child を起動し、stdio 上の JSON-RPC で Agent Client Protocol (ACP) を話します。child は task の canonical scope directory で、filter 済み環境変数 (PATH/HOME/proxy と Devin/Windsurf の credential 変数のみ) 付きで動きます。task record、ownership、lease、receipt、retention、scoped evidence は上記 Codex / OpenCode task と同じ契約です。task は完全な session instance と canonical working directory に所有され、別 session、別 process generation、別 scope から resume できません。

`devin_task_start` と `devin_task_control` には opaque な `operation_id` が必須です。control action は型付きの `steer` / `resume` / `interrupt` だけです。steer は保持済み ACP session に追加の `session/prompt` を送り、resume は agent が `loadSession` capability を advertise する場合に限り `session/load` で reattach します (advertise が無ければ replay せず fail closed)、interrupt は `session/cancel` を送ります。各 `session/prompt` は blocking の ACP request で、その `stopReason` result が task status に対応します (`end_turn` は completed、`cancelled` は interrupted、それ以外は `retryable_failed`)。送信済みの可能性がある turn の応答を失った場合は replay せず保持済み session state から reconcile します。agent 側の `session/request_permission` は auto-approval ではなく Temote-local approval console 経由の `waiting_approval` task state として表面化します。terminal report は他の delegation backend と同じ bounded report contract の下、蓄積した assistant message から抽出します。`ask` mode では同じ Devin provenance/scope/mutation metadata を伴う local approval が必要で、`agent` と `yolo` session では Temote-local prompt を skip します。`devin_task_get` の詳細は bounded・期限付き・session/scope限定の evidence だけです。

これは Temote の直接 `execute` sandbox と同じ OS-level boundary ではなく、experimental な stdio adapter です。ACP child は Devin service と直接通信し、認証は install 済み CLI 自身の credential state (`devin auth login`、`DEVIN_API_KEY`、または `WINDSURF_API_KEY`) を使います。host の Devin CLI build を end-to-end で検証するまでは、この surface を opt-in のままにしてください。

>>>>>>> 4a139de (delegation: add devin acp task backend)
### 構造化ローカルエージェント broker

`local_agent_run({session_id, agent, task, cwd?, access, model?, effort?, profile?})` は、インストール済みの Codex または OpenCode を構造化された broker 経由で1回実行します。`model` は adapter の model identifier、`effort` は Codex 専用の reasoning effort 名（他 agent では拒否）、`profile` は child に適用する名前付き provider/auth profile を選びます。
`worktree: {branch, task?}` を指定した場合、caller は path を一切渡しません。Temote が selected session workspace から canonical repository を解決し、`<configured src root>/worktrees/<repo>/<task>` を自ら導出して、その repository と branch の検証済み managed worktree だけを再利用し、存在しなければ承認済みの managed-worktree 経路で作成します。`cwd` と `worktree` の同時指定は拒否し、検証済み workspace は agent 起動直前に再検証します。`<repository>/.wt/<name>` や `<src>/<repo>-*` などの legacy worktree はこの選択で adopt・移動・削除しません。
`agent` は `codex` と `opencode` に限定し、呼び出し側が渡せるのは bounded な task と access mode だけです。
実行ファイル、raw argv、environment、network policy は Temote が構築し、caller から指定できません。
実装対象の non-interactive CLI contract は、インストール済み CLI の help 出力で検証します。

指定した `cwd` は canonicalize し、symlink 解決後も yolo を含むすべての session で permitted root 内に限定します。
permitted root は選択可能な `cwd` の範囲を認可するものであり、child に自動公開する path の一覧ではありません。
Temote 側の agent profile では、選択した canonical `cwd` だけを agent workspace として再公開します。
`workspace_write` では選択 cwd だけを書き込み可能にし、`read_only` では選択 cwd を読み取り専用にします。
他の permitted root は agent に自動公開しません。
どちらの mode でも agent の state と cache は毎回専用 directory に分離して書き込み可能にし、選択 workspace 以下のすべての `.git`、`.agents`、`.codex`（nested を含む）は保護したままにします。

`ask` と `yolo` では local agent の request が local approval boundary を通ります。`agent` では otherwise-valid な構造化 request が Temote 側の local approval prompt だけを省略し、以下の broker contract は変わりません。
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

### 構造化 developer tool broker

`dev_tool_run({session_id, tool, operation, args?, cwd?})` は、検証済みの Cargo / Vite+ / uv / npm / pnpm / Go operation を developer broker 経由で実行します。caller は executable や raw host command を指定できません。cwd は permitted root 内に canonicalize し、child output は bounded、長時間 operation は通常の `job_id` を返します。

operation class:

- offline development（`cargo fmt|check|clippy|test|build`、`vp check|lint|fmt|format|test|build|pack`）は network 無効の developer sandbox で実行し、workspace write と限定的な tool cache/state write だけを許可します。
- dependency/network（`cargo fetch|install|update`、`vp install|add|update|outdated|info|rebuild`）は明示的に分類された network profile を使い、write scope は同じです。
- package-manager の first slice として `uv lock`、`npm install|ci|update|ping|outdated`、`pnpm install|fetch|update|outdated`、`go mod_download`（実 argv は `go mod download`）を追加します。初期 contract では caller-supplied args を受け付けず、`uv lock` は `--no-build --no-python-downloads`、npm/pnpm の install/update 系は `--ignore-scripts` と `npm_config_ignore_scripts=true`、pnpm install/update/fetch はさらに `--ignore-pnpmfile` を broker 側で強制して project hook の実行も防ぎます。
- lifecycle build script は別の offline phase に分離します。`npm rebuild` と `pnpm rebuild_pending`（実 argv は `pnpm rebuild --pending`）は developer sandbox の network を無効にしたまま実行するため、依存取得時には script を止め、必要な build script だけを後段で outbound network なしに実行できます。
- `vp run|exec|dlx`、`vp upgrade|implode`、その他の未知 operation は offline/safe path に入れず拒否します。

`ask` では検証済み operation に local approval が必要で、`agent` では local approval console なしで実行し、`yolo` は従来の local behavior を維持します。分類と containment の規則はどの mode でも同一です。

### Delegation backend (ローカル CLI)

`temote-mcp delegate --backend codex|opencode ...` は、ローカル CLI から bounded な非対話 delegation を1回実行し、bounded な JSON result を1つ出力します。OpenCode は明示的な `--session <id>` resume も受け付けます。resume 前に bounded な read-only `opencode session list --format json` preflight を行い、session の canonical directory が現在の canonical delegation directory と一致する場合だけ `run` を起動します。metadata の欠落、曖昧さ、不正 JSON、サイズ超過、probe失敗、directory不一致は fail closed で、`run` より前に拒否します。`--fork` を使うには `--session` が必要で、指定した session の context を継承した新しい session を開始します。`run` の前に、同じ fail closed の directory preflight を親 session に対して実行します。`--continue` と `--attach` はサポートしません。OpenCode backend の executable は次の順で解決します。

1. `TEMOTE_OPENCODE_BIN` が設定されている場合、既存の実行可能な regular file への絶対 path でなければなりません。symlink は canonical target に解決します。PATH より優先されます。
2. 未設定の場合は PATH から `opencode` を解決します。

明示された `TEMOTE_OPENCODE_BIN` が不正（空、相対 path、存在しない、regular file でない、実行可能でない）な場合は fail closed とし、PATH 上の別 executable へ暗黙に fallback しません。設定された path は diagnostics や error に出力しません。`temote-mcp delegate diagnose --backend opencode` が示すのは `available` / `unavailable`、source (`env_override` / `path` / `invalid_override`)、invalid override の bounded な reason だけです。`TEMOTE_OPENCODE_BIN` は parent process が読むだけで、OpenCode child の environment には渡しません。

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
- `git_push_tag`: exact local commit SHA を configured remote の `refs/tags/<tag>` へ push する。`expected_remote_sha` 省略時は create-only、指定時は remote tag がその exact old SHA の場合だけ更新する。lightweight remote tag ref に限定し、任意 refspec / URL / annotated tag 作成 / unconditional force は受け付けない
- `git_branch_create`: `HEAD` または validated repository-local/fetched ref から local branch を1つ作成する。current worktree は切り替えず、force/reset/refspec/URL input は受け付けない
- `git_branch_delete`: exact local branch を merged-only semantics で削除する。current branch、いずれかの worktree で checkout 中の branch、unmerged branch、存在しない branch は拒否し、force-delete option は公開しない
- `git_remote_branch_delete`: configured push destination が1つだけの remote から exact `refs/heads/<branch>` を、review 済み `expected_remote_sha` が引き続き一致する場合にだけ削除する。live remote の symbolic `HEAD` を default branch の authority とし、GitHub destination は live branch metadata の `protected=false`、それ以外は repository-local の `temote.remote.<remote>.protectedBranch` policy を要求する。default/protection state が欠落・曖昧・取得不能なら fail closed とする。delete ref と exact force-with-lease は Temote が内部構築し、任意 URL/refspec、wildcard、tag、複数 push destination、unconditional force は受け付けない
- `git_switch`: validated existing local branch へ force/reset/stash なしで切り替える。dirty file の上書きが必要なら Git 自身が拒否し、Temote は worktree を変更しない
- `git_worktree_add`: linked worktree を `<repository>/.wt/<name>` にだけ作成する。`base` 指定時は validated repository-local commit から branch を新規作成し、省略時は existing local branch を attach する。任意 destination path / force option は受け付けない
- `git_worktree_create`: linked worktree を selected repository の exact managed root（`<configured src root>/worktrees/<repository>/<task>`。通常の `TEMOTE_MCP_ROOTS='src=~/src'` 構成では `~/src/worktrees/<repo>/<task>`）配下にだけ作成する。canonical repository は configured `src` named root 直下の exact 1 directory でなければならない。attach できるのは validated existing local branch のみで、新規 branch を作る場合は先に `git_branch_create` を使う。`task` は optional で、省略時は branch の `/` を `-` に潰して導出する。caller は filesystem path / `cwd` / `base` を指定できない。local approval より前の処理はすべて read-only で、作成成功は trusted managed root と repository identity に対して再検証してから報告する。absolute path / traversal / separator / option-like value / control character / symlink escape / 既存 target はすべて拒否する。`<repository>/.wt/<name>` や `~/src/<repo>-*` などの legacy worktree は move / adopt / reuse / delete しない
- `git_worktree_list`: selected repository の registered worktree を `primary` / `managed` / `legacy` に分類して列挙する。`managed` は trusted managed root（configured `src` root の exact path にある normal directory で、symlink / swapped path でないこと）への canonical containment と canonical common Git directory / primary checkout の一致を要求し、検証できないものは `legacy` として fail closed する。read-only であり worktree を変更しない
- `github_workflow_dispatch`: 選択した configured `github.com` remote からだけ GitHub repository を解決し、numeric workflow ID または `.yml` / `.yaml` filename を exact unqualified branch/tag ref で dispatch して、作成された workflow run ID を返す。repository-local Git credential mapping が helper を明示 reset したうえで `!gh git credential --managed` を選択し、`credential.useHttpPath=true` である場合だけ利用する。ambient な active `gh` account へは fallback しない。approval 後に exact repository credential を内部解決し、bounded な GitHub REST request にだけ利用する。継承 `GH_TOKEN` / `GITHUB_TOKEN` 系は sensitive として扱い続け、global `gh auth` state は変更せず、token 値は返さない
- `github_workflow_run_get`: 同じ repository-scoped credential mapping を使い、configured GitHub repository の exact workflow run ID を bounded status として読む。`status=completed` になるまでこの tool を poll し、terminal result は `conclusion` で判定する。raw log/artifact は取得しない

remote Git 操作は host operation なので、通常 session ではローカル承認が必要です。`git_worktree_create` も対応する host-side workspace operation であり、managed root policy と validation はどの permission mode でも同じです。

GitHub HTTPS remote では、`git_fetch` / `git_pull` / `git_push` / `git_push_tag` と `github_workflow_*` / `github_pr_*` の各 tool の前に repository-local の managed credential mapping が必要です。host 上で clone ごとに一度だけ設定します。

```sh
git config --local credential.helper ''
git config --local --add credential.helper '!gh git credential --managed'
git config --local credential.useHttpPath true
```

`credential mapping is unavailable` の error にも同じ手順が表示されます。opt-in の代替として、host-approved `ambient_git_credentials` session grant を使うと、mapping がない場合に限り ambient な Git credential に fallback できます。GitHub 以外や SSH remote の credential path はどの場合も変わりません。

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
