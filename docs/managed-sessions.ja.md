# managed session と named root

Temote の session lifecycle owner は `temote-mcp supervisor` の1つだけです。全 `RuntimeHandle`、durable lifecycle state、再接続可能な local approval broker をこの process が所有します。

`temote-mcp serve` / `temote-mcp up` は authenticated HTTP / ingress process に限定します。public `session_start` / `session_stop` は same-user の `0600` Unix control socket 経由で既存 local supervisor に委譲します。Tailscale local OAuth approval も同じ socket から `temote-mcp session console` へ転送し、public HTTP endpoint に approval attachment は公開しません。

tmux、Herdr、systemd などで lifecycle supervisor process を保持・再起動することはできますが、session 単位の正本にはしません。

## named root 設定

`TEMOTE_MCP_ROOTS` で MCP 上の logical namespace と host filesystem path を分離します。

```sh
TEMOTE_MCP_ROOTS='src=~/src'
```

複数 root は JSON を推奨します。

```sh
TEMOTE_MCP_ROOTS='{"src":"~/src","work":"~/work"}'
```

root 名は ASCII 英数字、`-`、`_` のみです。configured root 自体を最初に canonicalize するため、`~/src -> /Volumes/devstorage/Developer` のような管理者が選んだ alias は利用できます。一方、配下の symlink や `..` が canonical physical root の外へ解決される場合は拒否します。root 未設定時に HOME、`/`、cwd、repository cwd へ fallback しません。

## local session supervisor

foreground supervisor を1つ起動します。

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor
```

別 terminal から session を操作します。

```sh
temote-mcp session start mitsumori --path src/mitsumori-core
temote-mcp session list
temote-mcp session info mitsumori
temote-mcp session permission mitsumori status
temote-mcp session permission mitsumori allow /path/to/extra-root
temote-mcp session permission mitsumori revoke /path/to/extra-root
temote-mcp session permission mitsumori ask
temote-mcp session permission mitsumori yolo
temote-mcp session restart-policy mitsumori on-failure
temote-mcp session stop mitsumori
temote-mcp session restart mitsumori
```

`session list` は `starting` / `active` / `stopping` / `stopped` / `crashed` を表示し、live 判定時には runtime socket を probe します。metadata が `active` を示していても socket が死んでいる session を `active` とは表示しません。

`session info` では non-secret な `host_id`、cwd、permitted directory、permission mode、started/stopped timestamp、exit reason、last error、利用可能な場合は logical named-root path、restart policy、restart count、直近 restart time、pending restart time、restart limit reason を確認できます。

互換用の `temote-mcp start <id>` も残します。runtime をその terminal process が直接所有するのではなく、起動中の local supervisor に current directory の session 作成を依頼します。`--yolo` は local CLI のみで利用可能です。public MCP `session_start` から yolo mode は指定できません。

detached permission 管理は local-only で、同じ owner-only supervisor Unix socket を通します。`permission allow/revoke` は既存の canonical-path / symlink containment rule を維持し、session cwd の revoke は拒否します。`permission ask/yolo` は明示操作であり、これらの mutation は runtime を restart せず runtime state を失いません。同じ session/cwd を明示 restart した場合、persist 済み permitted root は復元されます。

## approval console attachment

approval input は別 attachment として接続します。

```sh
temote-mcp session console
```

approval console は runtime owner ではありません。stdin EOF、Ctrl-C、PTY disconnect、terminal close では console だけが detach し、session runtime は生存します。console 不在中の approval-required operation は fail closed します。後から `session console` を再実行すれば、以後の approval request を処理できます。

HTTP `serve/up` は独自の approval console を持たず、session runtime も所有しません。`serve/up` は起動時に local control protocol version を検証し、lifecycle supervisor の upgrade/restart が必要な場合は fail closed します。Tailscale OAuth approval と runtime の host approval は同じ再接続可能な `temote-mcp session console` を使います。HTTP origin / ingress が再起動しても session runtime は lifecycle supervisor 配下で生存します。

## runtime と failure isolation

session Unix socket は MCP operation と host bridge の runtime boundary として維持します。local CLI session と HTTP managed session は、sandbox permission、approval state、1Password bridge、kintone bridge、metadata、socket lifecycle を同じ runtime 実装で処理します。

connection-local failure は runtime から隔離します。Broken pipe、connection reset、malformed message、oversized message、read timeout、client disconnect、response write failure はその connection だけを終了します。特に probe response と yolo approval response の write failure は runtime loop へ伝播しません。

listener failure、runtime task panic/join failure など core runtime の予期しない終了は、その session に限って runtime-fatal とします。monitor は `crashed`、`stopped_at`、exit reason、last error を保存します。1 session の failure が同じ supervisor の他 session を停止させることはありません。

## lifecycle state の永続化

通常の session metadata に加えて private lifecycle state file を保持します。

```text
starting -> active -> stopping -> stopped
                    \-> crashed
```

明示的な graceful stop は `stopped`、予期しない終了は `crashed` です。local supervisor の起動時には stale socket を処理し、live runtime を示す metadata に対して socket が死んでいれば `crashed` に reconcile します。

restart policy の既定値は安全側の `never` です。`temote-mcp session restart-policy <id> on-failure` で、予期しない runtime failure に限って automatic restart を有効化できます。graceful stop は再起動しません。automatic restart は 1, 2, 4, 8, 16 秒の bounded exponential delay を使い、5回で limit に達して `crashed` に確定します。lifecycle state には `restart_count` / `last_restart_at` / `next_restart_at` / `restart_limit_reason` を保存します。start 時に capture した environment は supervisor memory のみに保持して永続化しません。そのため supervisor process 自体が再起動した後は pending な credential-bearing automatic restart を暗黙再開せず、理由を残して `crashed` のままにし、明示的な `session restart` を要求します。

## HTTP managed session

認証済み direct HTTP MCP client は以下を利用できます。

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
session_stop(session_id="my-project")
```

HTTP managed session は常に `yolo=false` です。既存の approval-gated host operation は引き続き approval が必要です。lifecycle supervisor は HTTP から作成した runtime を memory 内で識別し、public `session_stop` はその集合だけを停止できます。local CLI / yolo session は同じ supervisor 配下でも remote から停止できません。HTTP ownership は session metadata に permission として永続化しません。

`session_list` / `session_info` は active だけでなく stopped / crashed の durable state も表示します。それ以外の session-bound MCP tool は従来どおり live runtime socket を要求します。

`session_start` / `session_stop` は authenticated direct HTTP `serve` endpoint だけで公開します。direct `temote-mcp up` は public endpoint ごとに single-host であり、1 endpoint は1つの local lifecycle supervisor と host-local session store に対応します。同じ Cloudflare Tunnel token / hostname を複数 direct-ingress host で同時利用する構成は、Cloudflare replica routing が session-aware ではないため非対応です。stable な non-secret diagnostic identity には `TEMOTE_MCP_HOST_ID` を設定し、未設定時は OS hostname を使います。1つの public endpoint から複数 Temote host へ route する場合は `temote-mcp gateway-agent` + Worker/Durable Objects gateway を使用します。gateway の generation/lease routing contract は変更しません。公開 endpoint で `without_sandbox` を除外する既存境界も維持します。

## optional terminal integration

Herdr や tmux で `temote-mcp supervisor` の terminal を保持・整理することはできます。ただし optional な UI / process-retention layer に留め、session ownership、lifecycle metadata、crash detection、restart command の正本は Temote とします。
