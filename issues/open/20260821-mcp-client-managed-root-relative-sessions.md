# MCP client から root-relative path で session lifecycle を管理できるようにする

Status: open
Model: gpt-5.6-sol
Created: 2026-08-21
Updated: 2026-08-21
Branch: main

## 概要

`just up` で Temote MCP の HTTP origin と Cloudflare Tunnel が常駐している場合、MCP client 側から `session_start` を呼び、host 側で事前設定した root directory からの相対パスを指定して session を開始できるようにする。

現在は `session_list` / `session_info` が MCP tool として存在する一方、session 作成は対象ディレクトリへ `cd` した上で `temote-mcp start` または `just start` を人間が別 terminal から実行する必要がある。この非対称性を解消し、`just up` を常駐 host supervisor として利用できるようにする。

例:

```text
TEMOTE_MCP_ROOT_DIR=/Volumes/devstorage/Developer
just up
```

MCP client:

```json
{
  "path": "twin",
  "session_id": "twin"
}
```

期待結果:

```json
{
  "session_id": "twin",
  "cwd": "/Volumes/devstorage/Developer/twin",
  "status": "active",
  "yolo": false
}
```

## 背景

現行 session は単なる cwd metadata ではない。`temote-mcp start` は session ごとの Unix socket、sandbox root、permission state、1Password / kintone bridge、ローカル approval UI を保持する独立 runtime になっている。

そのため HTTP handler から単純に `temote-mcp start` を background spawn すると、session process が読むべき stdin / approval UI が失われる。remote-created session でも既存の sandbox / approval semantics を弱めないため、session runtime と terminal UI を分離し、`just up` 側で複数 session を管理できる supervisor を持たせる。

## 目標

- `just up` 済みなら MCP client から session を開始できる。
- client が指定する path は host 設定の root directory からの相対パスだけにする。
- session ID は省略可能で、既存と同様に UUID を生成できる。
- 作成した session は既存 `session_list` / `session_info` / tool calls から通常 session と同様に扱える。
- MCP client から session を明示停止できる。
- remote-created session でも通常モードの sandbox、path restriction、host-operation approval を維持する。
- CLI `temote-mcp start` と MCP `session_start` が session runtime を二重実装しない。

## 非目標

- MCP client から `--yolo` を有効化する機能は追加しない。
- 任意の absolute path を remote client に公開しない。
- root directory 外への symlink escape を許可しない。
- session start を permission escalation の代替にしない。
- この issue では Cloudflare Workers + Durable Objects gateway 経由の remote session creation は扱わない。multi-host では host selection / routing contract が別途必要になる。
- Windows native 対応は追加しない。

## MCP surface

最低限、次を追加する。

### `session_start`

Input:

```json
{
  "path": "twin",
  "session_id": "twin"
}
```

- `path` は required。
- `path` は `TEMOTE_MCP_ROOT_DIR` からの relative path。
- absolute path は拒否する。
- `session_id` は optional。
- `yolo` 引数は持たせない。

Output は少なくとも次を含む。

```json
{
  "session_id": "twin",
  "cwd": "/Volumes/devstorage/Developer/twin",
  "status": "active",
  "yolo": false
}
```

### `session_stop`

Input:

```json
{
  "session_id": "twin"
}
```

- supervisor が管理している session を graceful stop する。
- inactive / unknown session の semantics を明示し、idempotent にできるなら idempotent にする。
- CLI から独立起動された session を remote client が任意 kill できる設計にはしない。ownership を区別する。

### 既存 tools

- `session_list`
- `session_info`

は managed session も既存 session と同じ形式で返す。

## root directory contract

新しい host-side configuration として `TEMOTE_MCP_ROOT_DIR` を追加する。

例:

```text
TEMOTE_MCP_ROOT_DIR=/Volumes/devstorage/Developer
```

`session_start(path)` は次の手順で target cwd を解決する。

1. configured root を canonicalize する。
2. `path` が absolute なら拒否する。
3. `root.join(path)` を canonicalize する。
4. target が directory であることを確認する。
5. canonical target が canonical root 自身、またはその descendant であることを確認する。
6. root 外を指す `..` traversal / symlink escape を拒否する。

文字列レベルの `..` 禁止だけを security boundary にしない。

`TEMOTE_MCP_ROOT_DIR` が未設定なら public/direct HTTP endpoint の `session_start` は利用不可とし、任意の cwd や process cwd へ fallback しない。

## session supervisor

`just up` / `temote-mcp serve` を managed session の supervisor として扱う。

概念構成:

```text
just up
  |
  +-- cloudflared
  |
  `-- temote-mcp serve
        |
        +-- HTTP MCP server
        +-- local approval console
        `-- SessionSupervisor
              +-- session A runtime/socket
              +-- session B runtime/socket
              `-- session C runtime/socket
```

`just up` は、必要なら process 起動順を変更し、`temote-mcp serve` が foreground stdin を所有できるようにする。cloudflared は sibling/background process として lifecycle を連動させる。

## runtime refactor

現在 `approvals::start()` に混在している責務を最低限分離する。

- session metadata / socket lifecycle
- session request handling
- approval state
- bridge state
- terminal input / rendering
- supervisor lifecycle

CLI `temote-mcp start` は reusable session runtime を single-session terminal UI で起動する。

`temote-mcp serve` から作成した session は同じ reusable runtime を supervisor-managed mode で起動する。

sandbox / Git / bridge 実処理を CLI 用と MCP 用で複製しない。

## approval semantics

remote-created session でも通常 session の approval semantics を弱めない。

- sandboxed read/write/execute の既存制約を維持する。
- Git network operations、1Password service-account command、kintone bridge など既存 approval-gated operation は引き続き local approval を要求する。
- remote MCP client 自身が approval を自己承認できる tool は追加しない。
- `session_start` に `yolo` option を追加しない。

複数 managed session がある場合、local approval console は request がどの `session_id` に属するかを明示する。

permission root の変更を supervisor console から扱う場合も、対象 session を曖昧にしない syntax / state model にする。既存 `/permission` semantics を壊さず再利用できる構成を優先する。

## ownership / lifecycle

session に supervisor ownership を持たせ、少なくとも次を区別できるようにする。

- CLI-managed session
- `serve` supervisor-managed session

`session_stop` は supervisor-managed session のみ remote stop できるようにする。

`temote-mcp serve` 終了時は自身が管理する child session runtime を graceful shutdown し、socket / session active state を stale に残さない。

同じ session ID の active session が存在する場合、`session_start` は明示的に conflict として失敗し、既存 session を silently replace しない。

## public endpoint security

`session_start` / `session_stop` は direct HTTP MCP endpoint の authenticated client 向けに公開してよいが、既存 Cloudflare Access authentication boundary を通過した request に限定する。

公開 endpoint から以下はできないことをテストする。

- absolute path で任意 directory を session root にする。
- `../` や symlink で configured root 外へ出る。
- `yolo` session を作成する。
- unmanaged CLI session を停止する。
- root 未設定時に process cwd 等へ fallback する。

## 実装タスク

- [ ] `TEMOTE_MCP_ROOT_DIR` の config 読み込みと validation を追加する。
- [ ] root-relative target resolver を追加し、traversal / symlink escape test を追加する。
- [ ] `approvals::start()` の session runtime と terminal UI を reusable component に分離する。
- [ ] `SessionSupervisor` を追加する。
- [ ] `temote-mcp serve` が managed session を保持できるようにする。
- [ ] `just up` で approval console の stdin が失われない process layout に変更する。
- [ ] MCP tool `session_start` を追加する。
- [ ] MCP tool `session_stop` を追加する。
- [ ] managed / unmanaged session ownership を表現する。
- [ ] `session_list` / `session_info` が managed session を正しく返すことを確認する。
- [ ] serve shutdown 時の managed session cleanup を実装する。
- [ ] direct HTTP/public dispatch で session lifecycle tools を検証する。
- [ ] README / README.ja.md / docs/usage* / Agent Skill を新しい workflow に更新する。

## 受け入れ条件

- [ ] `TEMOTE_MCP_ROOT_DIR=/tmp/projects just up` の状態で MCP client から `session_start(path="repo-a")` が成功する。
- [ ] 返された session ID を使って `session_info`, `read_file`, `execute` 等の既存 tool を呼べる。
- [ ] `session_list` に managed session が active として現れる。
- [ ] `session_stop` 後は session が active list から消える。
- [ ] absolute path は拒否される。
- [ ] `../outside` は拒否される。
- [ ] configured root 内 symlink から root 外へ出る path は拒否される。
- [ ] root 自身または root 内の実 directory は受理される。
- [ ] root 未設定時の `session_start` は fail closed する。
- [ ] MCP client から yolo session を生成できない。
- [ ] remote-created normal session の approval-gated operation は local console approval なしに実行されない。
- [ ] 複数 managed session の approval 表示に session ID が含まれ、取り違えない。
- [ ] active session ID collision は existing session を置換せず conflict になる。
- [ ] `temote-mcp serve` 終了時に managed session socket が stale active として残らない。
- [ ] 従来の `temote-mcp start <id>` workflow が regression なく動く。
- [ ] public endpoint の `without_sandbox` 非公開など既存 security boundary を弱めない。
- [ ] `cargo fmt --all -- --check` が成功する。
- [ ] `cargo test` が成功する。
- [ ] `cargo clippy --all-targets -- -D warnings` が成功する。
- [ ] `git diff --check` が成功する。

## live acceptance

可能なら temporary root を使って direct HTTP endpoint で end-to-end を残す。

```text
root/
  repo-a/
  repo-b/
outside/
```

最低限:

1. `session_start(repo-a)` success
2. `session_start(repo-b)` success
3. `session_list` で2件確認
4. repo-a に対する safe read / execute
5. `../outside` rejection
6. root 内 symlink -> outside rejection
7. approval-gated operation が local approval 待ちになることを確認
8. `session_stop(repo-a)`
9. serve shutdown
10. stale socket / active metadata が残らないことを確認

live acceptance のためだけに security semantics を緩めない。

## ドキュメント方針

README は利用者向けに短く保つ。

利用例は次を中心にする。

```text
TEMOTE_MCP_ROOT_DIR=~/Developer
just up
```

その後 MCP client が `session_list` を確認し、必要な project session がなければ `session_start` する workflow を Agent Skill に記載する。

内部 supervisor / approval routing の詳細は `docs/` とこの issue に残す。

## 変更履歴

`CHANGES.md` impact: no
