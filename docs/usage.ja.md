# Temote MCP の使い方

[English](usage.md)

## session

AI に触らせたいディレクトリで Temote MCP を起動します。

```sh
cd ~/src/my-project
temote-mcp start my-project
```

`session_list` で起動中の session を取得できます。session に対する tool では `session_id` が必要ですが、新規作成する `session_start` は例外です。`session_info` で working directory、許可 root、yolo mode を確認できます。

相対パスは session の working directory を基準に解決されます。

### `temote-mcp serve` から作る managed session

host で `TEMOTE_MCP_ROOTS` を設定し、`just up` を端末で常駐させます。単一 root は `name=path` 形式です。

```sh
TEMOTE_MCP_ROOTS='src=~/src' just up
```

複数 root は区切り文字の ad-hoc list ではなく JSON object を使います。

```sh
TEMOTE_MCP_ROOTS='{"src":"~/src","work":"~/work"}' just up
```

client は `session_list` を確認し、必要なら `session_start(path="src/project")`、続いて `session_info` を呼びます。configured root 自体は canonicalize されるため、`~/src -> /Volumes/devstorage/Developer` のような host alias は利用できます。一方、その配下の symlink や `..` が canonical physical root の外へ解決される場合は拒否されます。roots 未設定時は HOME、`/`、cwd、repository cwd へ fallback しません。

`session_stop` で停止できるのは現在の `serve` supervisor が作成した session だけです。別 terminal の `temote-mcp start` session は停止できません。managed session は常に non-yolo で、CLI session と同じ local approval gate を維持します。

## 許可 root

通常 session では起動ディレクトリだけが最初の許可 root です。session を起動した端末で変更します。

```text
/permission allow ../another-project
/permission revoke ../another-project
/permission list
/permission status
```

通常 session は、許可 root の外へ出る path、symlink、command `cwd` を拒否します。

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

実行中は `/permission ask` と `/permission yolo` で切り替えられます。

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
