# Temote MCP

[English](README.md)

Temote MCP は、手元のファイル・コマンド・一部のホスト連携を MCP ツールとして公開し、通常セッションではサンドボックスとローカル承認を維持する MCP サーバーです。

## インストール

ビルド済みバイナリは `cargo-binstall` で導入できます。

```sh
cargo binstall temote-mcp
temote-mcp doctor
```

ソースからビルドする場合は次を使います。

```sh
cargo install temote-mcp --locked
```

旧 `just up` 構成を置き換える場合は、先に binary を更新し、migration を確認・適用してから現在の supervisor を起動します。

```sh
cargo binstall temote-mcp --force
temote-mcp migrate --dry-run
temote-mcp migrate
temote-mcp up --profile cloudflare
```

`migrate` は旧 runtime ownership に加えて、互換な旧 Cloudflare 設定も移行します。既存の `public.env` や内容の異なる既存 Tunnel token は上書きせず、checkout-local `.env` からは Temote が必要とする Cloudflare/runtime key だけをコピーします。別途 `temote-mcp start` した local session は停止しません。`temote-mcp up --profile cloudflare` も、移行先がまだ無い場合は同じ互換設定 migration を bootstrap できます。

macOS の canonical default は `~/.config/temote-mcp/public.env` です。以前の実装で誤って使われる可能性があった `~/Library/Application Support/temote-mcp/public.env` は migration source として認識します。Linux は通常の config directory semantics を維持し、`TEMOTE_MCP_ENV_FILE` を明示した場合はその path を優先します。

Apple Silicon Mac と Linux に対応しています。Intel Mac と Windows ネイティブは未対応で、gateway endpoint では WSL2 を利用できます。

## セッションを開始する

常駐 host では named root を設定し、lifecycle supervisor を1つ起動してから HTTP ingress を別 process として起動します。

```sh
# host の例:
# ~/src -> /Volumes/devstorage/Developer
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor

# 別 terminal/service から。既存構成は Cloudflare profile が default です。
temote-mcp up --profile cloudflare
# Tailscale Funnel + Temote local OAuth を使う場合:
# temote-mcp up --profile tailscale
# outbound-only の OpenAI Secure MCP Tunnel を bootstrap / 利用する場合。
# 対応する環境変数が無ければ、どちらも terminal echo 無効の hidden prompt で key を入力します:
# temote-mcp openai setup --workspace-id <workspace-id>
# temote-mcp up --profile openai
```

Direct `temote-mcp up` ingress は **public endpoint ごとに single-host** が契約です。`TEMOTE_MCP_HOST_ID=ubuntu1` のような stable かつ non-secret な host ID を設定すると、startup / `doctor` / supervisor / session diagnostics で host ownership を明示できます。未設定時は OS hostname に fallback します。同じ Cloudflare Tunnel token / hostname を複数 Temote host で direct-ingress replica として同時利用する構成は非対応です。Cloudflare の replica routing は Temote の session ownership を認識せず、session state は host-local のままだからです。1つの public endpoint から複数 host を扱う場合は、[multi-host Cloudflare gateway](docs/gateway.ja.md) の `temote-mcp gateway-agent` + Worker/Durable Objects gateway を使用します。

認証済み MCP client からは次の順で利用します。

```text
session_list
session_start(path="src/my-project", session_id="my-project")
session_info(session_id="my-project")
```

managed session は常に通常の sandbox session です。`session_start` は named root からの相対 path のみ受け付け、yolo mode は指定できません。HTTP `serve/up` は owner-only Unix socket 経由で local lifecycle supervisor に session ownership と Tailscale OAuth approval を委譲します。承認は `temote-mcp session console` で行います。`temote-mcp down` が停止するのは HTTP origin / managed ingress だけで、lifecycle supervisor と session runtime は生存します。

local session は Temote の session supervisor を1つ起動し、その配下で runtime を管理します。

```sh
export TEMOTE_MCP_ROOTS='src=~/src'
temote-mcp supervisor

# 別 terminal から:
temote-mcp session start my-project --path src/my-project
temote-mcp session list
temote-mcp session info my-project
temote-mcp session permission my-project status
temote-mcp session permission my-project allow /path/to/extra-root
temote-mcp session restart-policy my-project on-failure
temote-mcp session console
temote-mcp session stop my-project
```

approval console は runtime owner ではなく attachment です。terminal close / stdin EOF でも session runtime は生存し、console 不在中の approval-required operation は fail closed します。console は再接続できます。lifecycle metadata には `starting` / `active` / `stopping` / `stopped` / `crashed` と crash reason / last error を保存します。`session list` は socket を probe するため、死んだ runtime を `active` と表示しません。detached permission 変更は owner-only supervisor socket 経由で行い、runtime を再起動しません。restart policy の既定値は `never` で、明示的な `on-failure` は bounded exponential backoff と最大5回の restart limit を使い、restart count / timestamp / limit reason を保存します。start 時に capture した credential は memory-only のため、supervisor process 自体の再起動後に credential-bearing auto restart を暗黙再開しません。その場合は明示的な `session restart` を使います。

互換用に `cd ~/src/my-project && temote-mcp start my-project` は current directory を local supervisor 配下で起動する shorthand として残します。先に `temote-mcp supervisor` が必要です。`--yolo` はこの local CLI path だけで利用でき、remote MCP `session_start` から yolo session は作成できません。local stdio client は `temote-mcp mcp` を起動します。

## Codex Plugin と Agent Skill

インストール済み Temote binary から local Codex を利用する場合は、binary-owned Plugin installation を標準経路にします。

```sh
temote-mcp codex plugin install
temote-mcp codex status
temote-mcp codex diagnose --json
```

installer は Plugin を `CODEX_HOME`（未指定時は `~/.codex`）配下へ配置し、Codex 設定で `temote-mcp@debug` を有効化します。さらに、install を実行した Temote executable の exact path を生成した MCP 設定と `.temote-mcp-bin` の両方へ固定します。`PATH` 上の別の `temote-mcp` へ暗黙に切り替えません。Temote を更新した場合は `temote-mcp codex plugin install` を再実行し、新しい binary/version へ Plugin を移します。削除は `temote-mcp codex plugin uninstall` です。install / uninstall 後、既に起動中の Codex session は再起動して disk 上の Plugin inventory と揃えます。 install / uninstall は排他制御付きの transaction として実行します。完全に検証した bundle を atomic swap し、Codex config は symlink を辿らず atomic replace します。uninstall は config を先に無効化してから bundle を削除します。`codex status --json` は recoverable transaction artifact、dangling config、disabled bundle、stale version を報告します。

repository root 自体も開発・確認用の local Codex Plugin として利用できます。`.codex-plugin/plugin.json` が既存の `skills/temote-mcp` を公開し、`.mcp.json` は `PATH` 上の `temote-mcp mcp` を起動します。

Plugin は意図的に薄く保ちます。session lifecycle、named-root resolution、sandbox、approval、OAuth、ingress は native Temote binary が引き続き所有します。local managed session を使う前に `temote-mcp supervisor` は通常どおり起動します。ChatGPT やその他の remote client は local stdio Plugin 経路ではなく、Cloudflare / Tailscale / OpenAI Secure MCP Tunnel profile を利用します。

Codex Plugin を利用しない Agent Skill 対応 coding agent には、同じ同梱 Skill を直接導入できます。

```sh
gh skill install f4ah6o/temote-mcp temote-mcp --scope user
```

必要に応じて `--agent codex`、`--agent claude-code` など対象 agent を指定します。

## 詳細ドキュメント

- [session と tool の使い方](docs/usage.ja.md)
- [managed session と named root](docs/managed-sessions.ja.md)
- [remote connection profile: Cloudflare / Tailscale / OpenAI Secure MCP Tunnel](docs/public-http.ja.md)
- [1Password / kintone 連携](docs/integrations.ja.md)
- [multi-host Cloudflare gateway](docs/gateway.ja.md)
- [Linux sandbox と crates.io packaging](docs/linux-sandbox.ja.md)
- [build / test / release](docs/development.md)

repository checkout では `just up` / `just down` が checkout の binary を build・選択して CLI へ委譲する開発用 wrapper です。インストール済み binary の運用に `just` は必要ありません。

このリポジトリを編集する coding agent 向けの指示は [AGENTS.md](AGENTS.md) にあります。

## 由来とライセンス

このプロジェクトは [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) から派生しています。**Temote** の名前は、[@mr_konn が「remote」の対義語として提唱した「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46) に着想を得ています。詳細な attribution は [THIRD_PARTY_NOTICES.ja.md](THIRD_PARTY_NOTICES.ja.md) を参照してください。

ライセンスはリポジトリ内の MIT / Apache-2.0 ライセンスファイルに従います。
