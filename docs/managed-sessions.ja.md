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
temote-mcp session stop mitsumori
temote-mcp session restart mitsumori
```

`session list` は `starting` / `active` / `stopping` / `stopped` / `crashed` を表示し、live 判定時には runtime socket を probe します。metadata が `active` を示していても socket が死んでいる session を `active` とは表示しません。

`session info` では cwd、permitted directory、permission mode、started/stopped timestamp、exit reason、last error、利用可能な場合は logical named-root path、restart policy を確認できます。

互換用の `temote-mcp start <id>` も残します。runtime をその terminal process が直接所有するのではなく、起動中の local supervisor に current directory の session 作成を依頼します。`--yolo` は local CLI のみで利用可能です。public MCP `session_start` から yolo mode は指定できません。

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

第一段階の restart policy は安全側の `never` です。`temote-mcp session restart <id>` による manual restart は stopped / crashed / active session に利用できます。bounded backoff / rate limit を備えた自動 `on-failure` restart は別 issue で管理します。

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

`session_start` / `session_stop` は authenticated direct HTTP `serve` endpoint だけで公開します。Cloudflare Workers + Durable Objects multi-host gateway には公開せず、この lifecycle 変更で host-selection contract も追加しません。公開 endpoint で `without_sandbox` を除外する既存境界も維持します。

## optional terminal integration

Herdr や tmux で `temote-mcp supervisor` の terminal を保持・整理することはできます。ただし optional な UI / process-retention layer に留め、session ownership、lifecycle metadata、crash detection、restart command の正本は Temote とします。
