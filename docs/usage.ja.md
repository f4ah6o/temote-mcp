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

`session list` では durable な `starting` / `active` / `stopping` / `stopped` / `crashed` を確認できます。`session info` では working directory、permitted root、permission mode、timestamp、exit reason、last error を確認できます。死んでいる、または liveness が曖昧な socket を暗黙に active とは扱いません。manual restart は `temote-mcp session restart <id>` で行えます。自動 restart は現時点では有効化しません。

互換用に `cd ~/src/my-project && temote-mcp start my-project` も利用できます。これは起動中の local supervisor に current directory の session 作成を依頼します。`temote-mcp start my-project --yolo` は意図的に制限を外す local-only form として残します。

相対 path は session の working directory を基準に解決されます。

### `temote-mcp up` から作る managed session

host で `TEMOTE_MCP_ROOTS` を設定し、`temote-mcp up` を端末で常駐させます。単一 root は `name=path` 形式です。

```sh
TEMOTE_MCP_ROOTS='src=~/src' temote-mcp up
```

複数 root は区切り文字の ad-hoc list ではなく JSON object を使います。

```sh
TEMOTE_MCP_ROOTS='{"src":"~/src","work":"~/work"}' temote-mcp up
```

client は `session_list` を確認し、必要なら `session_start(path="src/project")`、続いて `session_info` を呼びます。configured root 自体は canonicalize されるため、`~/src -> /Volumes/devstorage/Developer` のような host alias は利用できます。一方、その配下の symlink や `..` が canonical physical root の外へ解決される場合は拒否されます。roots 未設定時は HOME、`/`、cwd、repository cwd へ fallback しません。

`session_stop` で停止できるのは現在の `serve` supervisor が作成した session だけです。HTTP managed session は常に non-yolo で、従来どおり local approval gate を維持します。stopped / crashed metadata は `session_list` / `session_info` から確認できますが、それ以外の session-bound tool は引き続き active socket を要求します。HTTP supervisor と Tunnel の停止は `temote-mcp down` で行います。repository checkout の `just up/down` は、この CLI command に委譲する開発用 wrapper です。

## 旧 always-on runtime の migration

古い repository checkout の `just up` は `temote-mcp serve` と `cloudflared` を sibling process として起動し、2つの PID を `~/.cache/temote-mcp/up.pids` に記録していました。現在の installed deployment は `temote-mcp up`、lock 付きの単一 `up.pid`、child-process ownership を使います。binary を置き換えても、すでに実行中の process 自体は新しくなりません。

current binary を install した後、一度だけ legacy runtime state を migrate します。

```sh
cargo binstall temote-mcp --force
temote-mcp migrate --dry-run
temote-mcp migrate
temote-mcp up --profile cloudflare
```

migration は legacy state file を安全に検証し、signal 前に live PID の process name を確認します。別 process へ PID が再利用されている場合は fail-closed です。削除するのは stale legacy state、停止するのは検証済みの旧 `temote-mcp serve` + `cloudflared` pair だけです。`public.env`、`tunnel-token`、session metadata、socket、別途起動した `temote-mcp start <session>` process は変更しません。legacy state が無い状態で `temote-mcp migrate` を再実行しても no-op です。

## 許可 root

通常 session では canonical な起動 directory が最初の permitted root です。local named-root selection で対象 project directory を決め、remote `session_start` は administrator が設定した named root 配下しか解決できません。通常 session は permitted root の外へ出る path、symlink target、command `cwd` を拒否します。

従来の inline `/permission ...` terminal command UI は detached runtime の owner ではなくなったため、第一段階の supervisor control surface には載せていません。権限を広げる変更ではなく、runtime は persisted permitted root のまま fail closed します。

## command

`execute` は shell を介さず argv を実行します。通常 session では network 無効の sandbox 内で動きます。foreground timeout 内に終了すれば結果を直接返し、それ以上かかる場合は `job_id` を返します。

最初から background 実行する場合は `start_command` を使います。`poll_job` で完了を確認し、`stop_job` で停止できます。job は session に所属し、最大2時間で終了し、session 終了時にもキャンセルされます。1 session あたり同時に8 jobまで実行できます。

stdout/stderr の保持量は合計 1 MiB までで、超過時は truncated として返します。

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
