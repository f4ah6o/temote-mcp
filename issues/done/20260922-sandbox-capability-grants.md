# Sandboxed `agent` session 向けの host-approved capability grants

Status: done
Created: 2026-09-22
Priority: P1 developer workflow friction
Type: sandbox / permission / developer UX
Related:
- `issues/open/20260915-agent-development-network-access.md`
- `issues/open/20260908-live-acceptance-matrix.md`
- `issues/open/20260916-agent-mode-repository-triage-end-to-end.md`
- `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`

## Observed

`permission_mode=agent` (yolo=false, permitted_directories=[repo root]) の
Temote session 経由で、リモート Mac 上の madobe worktree に対する
「worktree 作成 → dev アプリ (native shell + vite + dev bridge) 起動 →
`just check-windows-live-classify` (dev bridge `127.0.0.1:17872` 経由) →
結果回収 → アプリ停止」の自動化が、sandbox 制限に複数回阻まれた。

1. `vite --host 127.0.0.1 --port 5173` が `listen EPERM`。macOS seatbelt は
   `(deny default)` で `network-bind` / `network-inbound` を一切許可していない。
   outbound connect / localhost connect は既に許可済みで非対称。
2. `git -C <repo> fetch` が `.git/FETCH_HEAD` への write を拒否
   (protected metadata は仕様どおり)。managed `git_fetch` は
   `GitHub repository credential mapping is unavailable` で失敗し、
   設定手順が error / docs に導線として存在しない。
3. session root 外への `mkdir` が `Operation not permitted`。
   host 側には `session permission allow` があるが、agent 側から
   承認を得る経路がない。
4. `/bin/ps` が `Operation not permitted` (`process-info*` は
   same-sandbox のみ)。dev server が listen しているかの診断ができない。
5. `dev_tool_run` に caller env を渡す経路がなく `MADOBE_UI_DIST_DIR`
   のような mode 選択変数が使えない。
6. `permission_mode` は session 生成時に固定。操作単位の escalation がない。

## Goal

上記の madobe live check flow が Temote 経由だけで完走する、または
host 側の操作が最大 1 つの承認ゲートに集約される。

## Design

新しい永続化 session capability grant (`Session.grants`) を導入する。
`permitted_directories` と同様に session metadata JSON に保存され、
restart でも引き継がれる。grant は必ず host 承認 (console) または
host CLI でのみ設定される。

```text
SessionGrants {
    listen_ports: Vec<u16>,           // TCP bind/listen 可能 port allowlist
    dev_tool_env_prefixes: Vec<String>, // dev_tool_run env name prefix allowlist
    ambient_git_credentials: bool,    // managed mapping 不在時の ambient credential 使用
}
```

### Tools / surface

- `session_permission_request` (new tool, public 含む): agent が
  `{listen_ports?, dev_tool_env_prefixes?, ambient_git_credentials?,
  directories?}` を1回の approval で要求する。承認後、session socket の
  `ApplyGrants` message で runtime が persist する。
- `execute` / `start_command`: `allow_loopback_listen: true` opt-in。
  agent mode のみ。grant 済み port のみ bind 可能。
- `dev_tool_run`: `env: {name: value}`。name は grant 済み prefix に
  マッチする必要がある。caller env は broker が設定する env より先に
  挿入されるため、`npm_config_ignore_scripts` 等の broker invariant は
  上書きできない。
- `port_check` (new tool): `{port: u16}` → host 側から
  `127.0.0.1:<port>` への TCP connect で `listening: bool` を返す。
  grant 済み port のみ probe 可能 (grants スコープに限定し、汎用
  port scanner にはしない。process/cwd 情報は持たない)。
- network Git tools (`git_fetch`/`git_pull`/`git_push`/`git_push_tag`)
  と REST tools (`github_*`): `ambient_git_credentials` grant で
  repo-local managed credential mapping の事前要求を skip する。
  `git_*` は unrestricted 実行のため ambient helper/credential や
  forward された `ssh-agent` がそのまま認証に使える。
  `github_*` REST は引き続き `gh git credential --managed get` で
  token を取得し、global `gh auth` state は変更しない。
- `temote-mcp session permission <id> grant|ungrant <kind> <value>`:
  host 側から直接 grant する CLI。

### Platform semantics

- macOS seatbelt: per-port `(allow network-bind (local tcp "*:PORT"))` +
  `(allow network-inbound (local tcp "*:PORT"))` を policy に追加。
  **seatbelt は bind を loopback のみに scope できない**
  (`local ip "127.0.0.1:*"` は parser が拒否し、`localhost` は全
  interface に解決される)。grant は「その port に限り任意 interface で
  listen 可能」であり、approval detail と docs がそれを正直に示す。
- Linux: `LocalAgent` policy は既に任意 bind/listen を許可している
  (seccomp では port scope 不可)。contract は uniform に validate するが
  platform enforcement は macOS のみ強化される。asymmetry は docs に明記。

### Non-goals

- yolo 永続化 / public endpoint からの mode 変更。
- arbitrary argv (`vp run`/`exec`) の開放。
- `.git` raw write の sandbox 内許可 (broker path のみ)。
- sandbox / redaction / bounded output の緩和。

## Acceptance criteria

- 既定は現行どおり deny。全 capability は opt-in + host approval のみ。
- out-of-root write / `.git` raw write / arbitrary argv / ask-mode network
  は引き続き拒否される (回帰 test で確認)。
- madobe live check flow が `session_permission_request` 1 回の承認で
  完走できる。
