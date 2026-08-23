# managed session と named root

`temote-mcp serve` は、認証済み direct HTTP MCP client から作成された複数の通常 session を supervisor として管理できます。

## named root 設定

`TEMOTE_MCP_ROOTS` で MCP 上の logical namespace と host filesystem path を分離します。

単一 root:

```sh
TEMOTE_MCP_ROOTS='src=~/src'
```

複数 root は区切り文字 list ではなく JSON object を使います。

```sh
TEMOTE_MCP_ROOTS='{"src":"~/src","work":"~/work"}'
```

root name は ASCII 英数字、`-`、`_` のみです。configured root path は先に canonicalize します。これにより administrator が明示した次のような root alias を許容します。

```text
~/src -> /Volumes/devstorage/Developer
```

canonical target が physical containment boundary になります。`src/foo` はその配下へ join した後 canonicalize し、directory であることと、root 自身または descendant であることを確認します。descendant symlink や `..` が physical root の外へ解決される場合は拒否します。roots 未設定時は HOME、`/`、process cwd、repository cwd へ fallback しません。

## runtime と supervisor

session socket は MCP operation と host bridge の runtime boundary として維持します。CLI session と managed session は、metadata、Unix socket lifecycle、approval state、1Password/kintone bridge state、cleanup を同じ reusable session runtime で処理します。

`temote-mcp start` は単一 runtime に従来の terminal UI を付けます。`temote-mcp serve` は `SessionSupervisor` と共有 local approval console を所有します。approval prompt には local `y/yes` または `n/no` を受け付ける前に、対象 `session_id`、cwd、operation を表示します。

managed session は常に `yolo=false` です。MCP `session_start` schema に yolo option はなく、余分な field も拒否します。Git network operation、1Password service-account command、kintone call など既存の approval-gated host operation は引き続き local approval を待ちます。

## ownership と lifecycle

supervisor は自身が作成した runtime handle だけを追跡します。`session_stop` はそれらだけを graceful shutdown できます。別 terminal で起動した CLI session は `session_list` に見えても、remote client から `session_stop` で停止できません。

active session ID collision は既存 runtime/socket を置換せず失敗します。`serve` 終了時は managed runtime をすべて drain し、metadata を inactive (`process_id = 0`) にして Unix socket を削除します。

`temote-mcp up` は HTTP server を foreground process にし、選択した connection child（`cloudflared`、`tailscale funnel`、`tunnel-client`）だけを管理します。これにより approval console が端末 stdin を所有し、Temote が所有する connection cleanup も同じ shutdown path に結合されます。Tailscale daemon や無関係な ingress / tunnel 設定は停止しません。`temote-mcp down` は同じ graceful shutdown を要求し、不要になった lifecycle state を削除します。

## endpoint scope

`session_start` / `session_stop` は認証済み direct HTTP `serve` endpoint だけで公開します。Cloudflare Workers + Durable Objects multi-host gateway では公開せず、この設計では host-selection contract を追加しません。公開 endpoint で `without_sandbox` を除外する既存境界も維持します。
