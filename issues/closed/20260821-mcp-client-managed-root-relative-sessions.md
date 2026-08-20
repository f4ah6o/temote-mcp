# MCP client から named root-relative path で session lifecycle を管理できるようにする

Status: closed
Model: gpt-5.6-sol
Created: 2026-08-21
Updated: 2026-08-21
Branch: main

## 概要

`just up` で Temote MCP の HTTP origin と Cloudflare Tunnel が常駐している場合、MCP client 側から `session_start` を呼び、host 側で事前設定した **named root** からの相対パスを指定して session を開始できるようにする。

現在は `session_list` / `session_info` が MCP tool として存在する一方、session 作成は対象ディレクトリへ `cd` した上で `temote-mcp start` または `just start` を人間が別 terminal から実行する必要がある。この非対称性を解消し、`just up` を常駐 host supervisor として利用できるようにする。

主用途として、host 側で次のように `~/src` が別 volume への symlink になっていても、client からは論理 namespace の `src/foo` と指定できることを重視する。

```text
~/src -> /Volumes/devstorage/Developer
```

host configuration:

```text
TEMOTE_MCP_ROOTS=src=~/src
just up
```

MCP client:

```json
{
  "path": "src/foo",
  "session_id": "foo"
}
```

期待結果:

```json
{
  "session_id": "foo",
  "cwd": "/Volumes/devstorage/Developer/foo",
  "status": "active",
  "yolo": false
}
```

## 背景

現行 session は単なる cwd metadata ではない。`temote-mcp start` は session ごとの Unix socket、sandbox root、permission state、1Password / kintone bridge、ローカル approval UI を保持する独立 runtime になっている。

そのため HTTP handler から単純に `temote-mcp start` を background spawn すると、session process が読むべき stdin / approval UI が失われる。remote-created session でも既存の sandbox / approval semantics を弱めないため、session runtime と terminal UI を分離し、`just up` 側で複数 session を管理できる supervisor を持たせる。

また、単一の physical root に containment をかけるだけでは、`~/src -> /Volumes/...` のような日常的な symlink mount を logical path `src/foo` として扱えない。そこで「logical root name」と「host 上の physical root」を分離する。

## 目標

- `just up` 済みなら MCP client から session を開始できる。
- client が指定する path は configured named roots から始まる logical relative path のみにする。
- `src/foo` のような stable logical path を使える。
- named root 自体が symlink でも、その symlink target を root boundary として安全に利用できる。
- named root 配下からさらに root 外へ出る traversal / symlink escape は拒否する。
- session ID は省略可能で、既存と同様に UUID を生成できる。
- 作成した session は既存 `session_list` / `session_info` / tool calls から通常 session と同様に扱える。
- MCP client から session を明示停止できる。
- remote-created session でも通常モードの sandbox、path restriction、host-operation approval を維持する。
- CLI `temote-mcp start` と MCP `session_start` が session runtime を二重実装しない。

## 非目標

- MCP client から `--yolo` を有効化する機能は追加しない。
- arbitrary absolute path を remote client に公開しない。
- configured named root 配下から root 外への symlink escape を許可しない。
- session start を permission escalation の代替にしない。
- この issue では Cloudflare Workers + Durable Objects gateway 経由の remote session creation は扱わない。multi-host では host selection / routing contract が別途必要になる。
- Windows native 対応は追加しない。

## MCP surface

最低限、次を追加する。

### `session_start`

Input:

```json
{
  "path": "src/foo",
  "session_id": "foo"
}
```

- `path` は required。
- `path` は `<root-name>` または `<root-name>/<relative-path>` の logical path。
- absolute path は拒否する。
- unknown root name は拒否する。
- `session_id` は optional。
- `yolo` 引数は持たせない。

Output は少なくとも次を含む。

```json
{
  "session_id": "foo",
  "cwd": "/Volumes/devstorage/Developer/foo",
  "status": "active",
  "yolo": false
}
```

可能なら `session_info` / `session_list` には debugging / observability 用に logical root/path も追加してよい。ただし既存 consumer を壊さない additive field にする。

### `session_stop`

Input:

```json
{
  "session_id": "foo"
}
```

- supervisor が管理している session を graceful stop する。
- inactive / unknown session の semantics を明示し、idempotent にできるなら idempotent にする。
- CLI から独立起動された session を remote client が任意 kill できる設計にはしない。ownership を区別する。

### 既存 tools

- `session_list`
- `session_info`

は managed session も既存 session と同じ形式で返す。

## named root contract

新しい host-side configuration として `TEMOTE_MCP_ROOTS` を追加する。

最低限、単一 root は次のように設定できるようにする。

```text
TEMOTE_MCP_ROOTS=src=~/src
```

複数 root も設定できる contract を定義する。具体的な separator / parser は安全かつ document しやすい形式を選ぶこと。パスに separator が含まれる曖昧な ad-hoc parser は避ける。必要なら config file / structured representation を採用してよい。

root name は MCP path namespace の first component として使うため、少なくとも ASCII alphanumeric、`-`、`_` 程度に制限し、`.`, `..`, slash、空文字は拒否する。

### root symlink semantics

named root の configured path 自体は symlink でよい。

例:

```text
~/src -> /Volumes/devstorage/Developer
```

起動時または初回使用時に configured root path を canonicalize し、その結果を **physical containment root** とする。

```text
logical root:  src
configured:    ~/src
canonical:     /Volumes/devstorage/Developer
```

この場合、client の `src/foo` は次として扱う。

```text
/Volumes/devstorage/Developer/foo
```

つまり root alias 自体の symlink traversal は host configuration として明示的に trusted とする。

一方、named root の descendant にある symlink が canonical root の外へ出る場合は拒否する。

```text
~/src/outside-link -> /private/tmp/outside
```

`src/outside-link` やその descendant は session root にできない。

### path resolution

`session_start(path)` は次の手順で target cwd を解決する。

1. path が absolute なら拒否する。
2. first component を root name として取得する。
3. configured named root を取得する。unknown root は拒否する。
4. configured root 自体を canonicalize して physical root を得る。
5. remaining components を physical root に join して target を canonicalize する。
6. target が directory であることを確認する。
7. canonical target が canonical physical root 自身、またはその descendant であることを確認する。
8. root 外を指す `..` traversal / descendant symlink escape を拒否する。

文字列レベルの `..` 禁止だけを security boundary にしない。

named roots が未設定なら public/direct HTTP endpoint の `session_start` は利用不可とし、HOME、process cwd、filesystem root などへ fallback しない。

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
- unknown root alias を使う。
- `../` や descendant symlink で configured physical root 外へ出る。
- `yolo` session を作成する。
- unmanaged CLI session を停止する。
- roots 未設定時に HOME / process cwd 等へ fallback する。

## 実装タスク

- [x] `TEMOTE_MCP_ROOTS` の config 読み込みと validation を追加する。
- [x] named root の structured parser / representation を追加する。
- [x] logical `<root>/<path>` resolver を追加する。
- [x] configured root 自体の symlink canonicalization を正式にサポートする。
- [x] traversal / descendant symlink escape tests を追加する。
- [x] `approvals::start()` の session runtime と terminal UI を reusable component に分離する。
- [x] `SessionSupervisor` を追加する。
- [x] `temote-mcp serve` が managed session を保持できるようにする。
- [x] `just up` で approval console の stdin が失われない process layout に変更する。
- [x] MCP tool `session_start` を追加する。
- [x] MCP tool `session_stop` を追加する。
- [x] managed / unmanaged session ownership を表現する。
- [x] `session_list` / `session_info` が managed session を正しく返すことを確認する。
- [x] serve shutdown 時の managed session cleanup を実装する。
- [x] direct HTTP/public dispatch で session lifecycle tools を検証する。
- [x] README / README.ja.md / docs/usage* / Agent Skill を新しい workflow に更新する。

## 受け入れ条件

host fixture:

```text
$HOME/src -> /tmp/temote-volume
/tmp/temote-volume/
  repo-a/
  repo-b/
  outside-link -> /tmp/outside
/tmp/outside/
```

configuration:

```text
TEMOTE_MCP_ROOTS=src=$HOME/src
```

最低限、次を満たす。

- [x] MCP client から `session_start(path="src/repo-a")` が成功する。
- [x] root alias 自体が symlink でも `src/repo-a` が canonical physical target に解決される。
- [x] `session_start(path="src")` で named root 自身を session root にできる。
- [x] 返された session ID を使って `session_info`, `read_file`, `execute` 等の既存 tool を呼べる。
- [x] `session_list` に managed session が active として現れる。
- [x] `session_stop` 後は session が active list から消える。
- [x] absolute path は拒否される。
- [x] unknown root alias は拒否される。
- [x] `src/../outside` 等で physical root 外へ出る path は拒否される。
- [x] `src/outside-link` のような descendant symlink escape は拒否される。
- [x] roots 未設定時の `session_start` は fail closed する。
- [x] MCP client から yolo session を生成できない。
- [x] remote-created normal session の approval-gated operation は local console approval なしに実行されない。
- [x] 複数 managed session の approval 表示に session ID が含まれ、取り違えない。
- [x] active session ID collision は existing session を置換せず conflict になる。
- [x] `temote-mcp serve` 終了時に managed session socket が stale active として残らない。
- [x] 従来の `temote-mcp start <id>` workflow が regression なく動く。
- [x] public endpoint の `without_sandbox` 非公開など既存 security boundary を弱めない。
- [x] `cargo fmt --all -- --check` が成功する。
- [x] `cargo test` が成功する。
- [x] `cargo clippy --all-targets -- -D warnings` が成功する。
- [x] `git diff --check` が成功する。

## live acceptance

可能なら temporary roots と symlink fixture を使って direct HTTP endpoint で end-to-end evidence を残す。

最低限:

1. `session_start(src/repo-a)` success
2. `session_start(src/repo-b)` success
3. `session_list` で2件確認
4. repo-a に対する safe read / execute
5. unknown root rejection
6. absolute path rejection
7. traversal rejection
8. descendant symlink escape rejection
9. approval-gated operation が local approval 待ちになることを確認
10. `session_stop`
11. serve shutdown
12. stale socket / active metadata が残らないことを確認

live acceptance のためだけに security semantics を緩めない。

## ドキュメント方針

README は利用者向けに短く保つ。

主例は次とする。

```text
~/src -> /Volumes/devstorage/Developer
TEMOTE_MCP_ROOTS=src=~/src
just up
```

その後 MCP client が `session_list` を確認し、必要な project session がなければ、たとえば `session_start(path="src/foo")` する workflow を Agent Skill に記載する。

内部 supervisor / approval routing / canonical containment の詳細は `docs/` とこの issue に残す。

## 変更履歴

`CHANGES.md` impact: no

## Completion evidence

Completed: 2026-08-21

### Implementation

- Added `TEMOTE_MCP_ROOTS` named-root configuration. A single root keeps the convenient `name=path` form; multiple roots use a JSON object so path separators are not overloaded.
- Configured root paths are expanded and canonicalized before use. The configured root itself may therefore be a symlink, while every requested target is canonicalized again and must remain the canonical root itself or a descendant.
- Added reusable session runtime plumbing shared by `temote-mcp start` and `temote-mcp serve`, plus a `SessionSupervisor` for serve-owned sessions.
- Added direct HTTP MCP `session_start(path, session_id?)` / `session_stop(session_id)`. Managed sessions are always `yolo=false`; extra fields such as `yolo` are rejected.
- `session_stop` tracks in-process supervisor ownership and refuses to stop an independently started active CLI session.
- Managed approval requests are routed to the local serve console and display `session_id`, cwd, and operation before a local allow/deny response is accepted.
- `just up` now keeps `temote-mcp serve` in the foreground and makes `cloudflared` its child, so serve owns terminal stdin and shuts down its managed sessions and Tunnel together.
- Cloudflare Workers/Durable Objects gateway dispatch does not advertise managed-session lifecycle tools; host selection remains out of scope. The public direct HTTP endpoint still excludes `without_sandbox`.
- Updated README / README.ja.md, detailed managed-session docs, public HTTP/usage docs, `.env.example`, and the Agent Skill workflow.

### Automated acceptance

`cargo test` passed with 83 tests. Relevant evidence includes:

- named-root parsing and strict root-name validation;
- configured root-alias symlink canonicalization;
- `src`, `src/repo-a`, and `src/repo-b` canonical target success;
- absolute path, unknown root, traversal, descendant symlink escape, nonexistent directory, and roots-unset rejection;
- authenticated direct HTTP `session_start` for two sessions, `session_list`, `session_info`, `read_file`, sandboxed `execute`, duplicate-ID rejection, and `session_stop`;
- rejection of a `yolo` field at the public session-start boundary;
- unmanaged active CLI runtime cannot be stopped by `session_stop`;
- supervisor shutdown removes sockets and writes inactive session metadata;
- approval-gated request remains pending until the local approval receiver responds, and the prompt identifies the correct session ID, cwd, and operation;
- existing Cloudflare Access unauthenticated rejection and public `without_sandbox` exclusion tests remain green.

### CLI regression / host observation

A freshly built `target/debug/temote-mcp start <id>` was run with stdin closed as a lifecycle regression check. It exited successfully (`exit 0`) and left no session socket behind. The traditional CLI startup output also retained the existing permission-mode and integration-status information.

The current host was inspected without modifying it. On this machine `~/src` is presently a real directory, while `~/src/local-mcp` is a descendant symlink to `/Volumes/devstorage/Developer/local-mcp`. Under the required semantics, configuring `src=~/src` must reject that descendant escape rather than treating it as a trusted root alias. Therefore no live validation weakened containment to make the current host layout pass. The requested root-alias case (`~/src` itself being the configured symlink) is covered by the canonicalization fixture and direct HTTP acceptance tests.

A live request through the production Cloudflare Access endpoint was not performed because doing so would require reconfiguring/restarting the currently connected host service or bypassing its authentication boundary. Direct HTTP routing is exercised with the authenticated test runtime instead; production Access authentication tests remain intact.

### Validation

All required final commands passed on the completed worktree:

```text
cargo fmt --all -- --check                       PASS
cargo test                                       PASS (83 passed, 0 failed)
cargo clippy --all-targets -- -D warnings        PASS
git diff --check                                 PASS
```

`just --list` also parsed the updated process-layout recipes successfully.

### CI follow-up

The first post-merge CI run (`32426742525`) exposed a Linux-only test-layout issue in the existing sandbox-helper lookup. During `cargo test --all-targets`, the test executable lives under `target/debug/deps/`, while the `codex-linux-sandbox` helper is built at `target/debug/`. The new managed-session HTTP `execute` acceptance was the first Linux CI path to exercise that lookup from a test binary.

The helper lookup now preserves the production sibling lookup and additionally recognizes Cargo's `deps/` test-binary layout, resolving only the adjacent profile directory's `codex-linux-sandbox`. The first fix run (`32427474144`) confirmed that `cargo test --all-targets` does not itself emit the normal helper executable; it only emits the helper's test harness under `deps/`. CI therefore builds the real `codex-linux-sandbox` binary before the Linux test step, matching the installed/runtime process layout while keeping the HTTP `execute` acceptance real rather than skipping it. No sandbox, approval, path-containment, or public-endpoint semantics were relaxed.

Post-fix local validation:

```text
cargo fmt --all -- --check                              PASS
cargo test --all-targets --all-features --locked       PASS (83 passed, 0 failed on macOS)
cargo clippy --all-targets --all-features --locked -- -D warnings  PASS
git diff --check                                        PASS
```

