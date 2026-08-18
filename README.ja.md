# local-mcp

[English](README.md)

**系譜:** 本リポジトリは [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) を起点として派生し、安全なリモートアクセス、gateway 運用、MCP bridge に重点を置いています。GitHub 上では fork metadata を持たないため、この関係を明示します。

**テモート / Temote:** 本プロジェクトは、[@mr_konn が「remote」の対義語として提唱した「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46) からも重要な着想を得ています。提唱者による英字表記とは扱わず、本リポジトリでは便宜上 **Temote** と表記します。例示用ホスト名 `temotemcp.example.com` もこの表記に基づきます。この命名上の着想は fork 元と並んで、本プロジェクトの成り立ちに重要なものとして明記します。

local-mcp は、ローカルファイル操作と sandbox 化されたコマンド実行を MCP tool として公開します。Web 検索や汎用ネットワークリクエスト機能は提供しません。通常モードの sandbox command はネットワークアクセスを無効化します。Linux では固定された Codex sandbox stack、macOS では local-mcp 独自の Seatbelt backend を使用します。

公開 HTTP endpoint は、オンデマンドの Cloudflare Tunnel を前提とします。

    ChatGPT Plus
        | Managed OAuth
        v
    Cloudflare Access -- Cloudflare Tunnel -- 127.0.0.1:8791
                                                   |
                                                   v
                                            local-mcp serve

## Build

    cargo build --release
    cargo install --path . --locked

既定 build には public HTTP と gateway-agent command が含まれます。`doctor`、`start`、`mcp` のみを含む local-only binary を作る場合は、既定の `network` feature を無効化します。

    cargo build --release --no-default-features --locked

Linux では local-mcp と sibling の `codex-linux-sandbox` binary を同じ directory に配置し、`bwrap` を PATH から利用できるようにしてください。macOS は system Seatbelt sandbox を使用します。Windows native は未対応です。

install 時は `--locked` を維持してください。Linux 向け Codex sandbox dependency は Git revision に固定されており、commit 済み lockfile を使わずに解決すると互換性のない prerelease dependency が選択される可能性があります。macOS では Codex sandbox crate を解決しません。

session 開始前や ChatGPT 接続前に host diagnostics を実行します。

    local-mcp doctor

Linux の `doctor` は `codex-linux-sandbox`、`bubblewrap`、user namespace、隔離 network namespace、実 sandbox command、shell command の runtime environment を確認します。必須 check が失敗すると non-zero で終了します。

## Release versioning

release は `Asia/Tokyo` timezone の CalVer `YYYY.MM.PATCH` を [`f4ah6o/calver-action`](https://github.com/f4ah6o/calver-action) で割り当てます。`main` history 内の commit に `latest` tag を移動すると release を要求できます。

    git tag -f latest <commit-to-release>
    git push -f origin latest

`.github/workflows/release.yaml` は次の prefixless CalVer tag を割り当て、release 専用 commit で `Cargo.toml` と `Cargo.lock` を更新し、normal/local-only build を検証して immutable tag を push します。この release-only commit は `main` に merge しません。

## Local sessions

各 session は、その session に許可する project directory から開始します。

    cd ~/src/local-mcp
    local-mcp start local-mcp

    cd ~/src/shuttle-rs
    local-mcp start shuttle-rs

明示的に制限を解除する場合は `--yolo` を付けます。

    local-mcp start local-mcp --yolo

YOLO mode では local-mcp の approval prompt と path root 制約が無効になり、command tool は local-mcp を実行している user の filesystem・environment・process・network 権限で host 上に直接実行されます。これは意図的に危険な mode です。`/permission ask` で通常 mode、`/permission yolo` で YOLO mode に切り替えます。

すべての tool call は `session_id` を明示します。ただし `session_list` は active session 発見用のため例外です。`session_info` は working directory と sandbox root を表示します。

追加 directory を許可する場合は session の local terminal で操作します。

    /permission allow ../another-project
    /permission revoke ../another-project
    /permission list
    /permission status

ChatGPT から Git を操作する場合、local commit には `git_add` と `git_commit`、remote synchronization には `git_fetch`、`git_pull`、`git_push` を使用します。remote Git tool は通常 approval-gated で、force push や arbitrary URL/refspec は受け付けません。`git_pull` は fast-forward-only です。

## Public HTTP endpoint

`.env.example` を repository 外へコピーし、Cloudflare Access の値を設定します。Tunnel token は credential なので file mode は 0600 を維持してください。

    install -d -m 700 ~/.config/local-mcp
    cp .env.example ~/.config/local-mcp/public.env
    chmod 600 ~/.config/local-mcp/public.env
    vi ~/.config/local-mcp/public.env

service が必要なときだけ別 terminal で起動します。

    # Terminal 1: local origin
    set -a
    . ~/.config/local-mcp/public.env
    set +a
    local-mcp serve

    # Terminal 2: on-demand remotely managed Tunnel
    set -a
    . ~/.config/local-mcp/public.env
    set +a
    cloudflared tunnel run --token "$LOCAL_MCP_TUNNEL_TOKEN"

    # Terminal 3+: project/session ごとに1つ
    cd ~/src/local-mcp
    local-mcp start local-mcp

`justfile` を使う場合は次の通りです。

    just build
    just doctor
    just env-check
    just up
    just down
    just serve
    just tunnel
    just start local-mcp
    just start shuttle-rs ~/src/shuttle-rs

公開 route の例は次です。

    https://temotemcp.example.com/mcp

Cloudflare 側では次を構成します。

1. remotely managed Tunnel を作成し、`temotemcp.example.com` を `http://127.0.0.1:8791` へ route します。
2. public hostname 全体を保護する **self-hosted Cloudflare Access application** を作成します。`/mcp` のみに制限しないでください。Managed OAuth discovery は host root の `/.well-known/` を使用します。
3. intended email account のみを許可する Allow policy を設定します。公開 endpoint に Bypass や Service Auth policy を追加しません。
4. self-hosted application の Advanced settings で Managed OAuth を有効にします。dynamic client registration、短い access-token lifetime、適切な grant session duration を設定します。ChatGPT redirect URI は次です。

       https://chatgpt.com/connector/oauth/*
       https://chatgpt.com/connector_platform_oauth_redirect

`AI controls > MCP servers` の portal registration は direct Tunnel route を保護する self-hosted application とは別物です。

Managed OAuth は Cloudflare Access が終端します。Rust origin は `Cf-Access-Jwt-Assertion` の signature、issuer、audience、expiry、subject、許可 email を検証します。`LOCAL_MCP_ACCESS_AUDIENCE` には `temotemcp.example.com` を保護する self-hosted application の AUD を設定してください。

接続前 probe の例です。

    curl -i -X POST https://temotemcp.example.com/mcp \
      -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'
    curl -i https://temotemcp.example.com/.well-known/oauth-authorization-server
    curl -i https://temotemcp.example.com/.well-known/oauth-protected-resource

未認証 MCP request は Cloudflare の `WWW-Authenticate` header 付き `401`、discovery endpoint は JSON metadata 付き `200` になる想定です。`530` は on-demand Tunnel または local origin が停止していることを示します。

## Limits and safety boundaries

- public request には valid Cloudflare Access assertion が必要です。
- `session_list` を除き、public tool は active `session_id` を必須とします。
- normal mode では file path、symlink target、command cwd は permitted root 内に制限されます。
- normal mode の sandbox command は network access を持ちません。
- `--yolo` では path と sandbox boundary が無効になり、host user 権限で command が実行されます。
- ordinary sandbox command は `.git` を書き換えません。local Git metadata の変更には専用 Git tool を使います。
- remote Git operation は dedicated tool を使い、`git_pull` は fast-forward-only、`git_push` は force option を公開しません。
- session あたり active sandbox job は最大4件です。
- stdout/stderr 合計は command あたり 1 MiB 上限です。
- background sandbox job は最長2時間、または session 終了時に cancel されます。
- runtime audit は tool、session、status、timing metadata を記録しますが、認証 email/subject、command argument/output は永続 audit log に保存しません。
- secret-file denylist は意図的に持ちません。`/home` 等の広い root を許可せず、必要な directory だけを明示してください。

## Local stdio mode

local MCP client が process を直接起動する場合は次を使用します。

    local-mcp mcp

この mode は Cloudflare Access HTTP endpoint とは独立しています。

## 1Password Environments MCP

local-mcp は official local 1Password Environments MCP server を bridge できます。macOS では通常 `/Applications/1Password.app/Contents/MacOS/1password-mcp`、Linux では `/opt/1Password/1password-mcp` を使用します。非標準 installation のみ `LOCAL_MCP_ONEPASSWORD_MCP` を指定してください。

公開/local endpoint は次の bridge tool を提供します。

- `onepassword_mcp_discover`: child server の resource と tool schema を列挙します。
- `onepassword_mcp_read_resource`: child server が公開した resource のみを読みます。
- `onepassword_mcp_call`: persistent stdio connection 経由で child tool を呼び出します。

normal session では mutating child tool は local-mcp の approval 対象です。secret handling は official 1Password MCP server が担当し、local-mcp は secret-reading API を追加しません。

### Service-account mode

unattended secret injection では `local-mcp start` に service-account token を渡します。

    OP_SERVICE_ACCOUNT_TOKEN='<service-account-token>' local-mcp start my-project

service-account token は session process が保持し、session JSON metadata には書き込みません。

追加 tool:

- `onepassword_service_account_status`: token の存在と `op whoami` 成功可否を token 非表示で確認します。
- `onepassword_service_account_run`: `op run` 経由で command を実行します。secret reference は `op://...` を使用し、plaintext value は拒否します。

## kintone MCP Server

local-mcp は official [`@kintone/mcp-server`](https://github.com/kintone/mcp-server) も bridge できます。host 側に CLI を install してください。

    npm install -g @kintone/mcp-server

credential は `local-mcp serve` や `gateway-agent` ではなく **`local-mcp start` process** に渡します。

    KINTONE_BASE_URL='https://example.cybozu.com' \
    KINTONE_API_TOKEN='<api-token>' \
    local-mcp start my-project

username/password authentication も利用できます。

    KINTONE_BASE_URL='https://example.cybozu.com' \
    KINTONE_USERNAME='<username>' \
    KINTONE_PASSWORD='<password>' \
    local-mcp start my-project

公開/local endpoint は `kintone_mcp_status`、`kintone_mcp_discover`、`kintone_mcp_call` を提供します。credential value や tenant URL を status から返しません。

## Serverless multi-host gateway

optional [`gateway/`](gateway/) deployment は1つの Cloudflare Workers MCP endpoint を公開し、`session_id` ごとに Durable Object 経由で host へ route します。Mac、Linux、Windows/WSL2 endpoint の `local-mcp gateway-agent` は outbound-only HTTPS long poll を行うため、host ごとの inbound port や Tunnel は不要です。

典型的な endpoint command:

    # Mac
    local-mcp start mac-main
    local-mcp gateway-agent --session-id mac-main

    # Windows interim path, inside WSL2
    local-mcp start windows-wsl2-main
    local-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2

`LOCAL_MCP_GATEWAY_URL`、`LOCAL_MCP_GATEWAY_HOST_TOKEN`、必要に応じて Cloudflare Access service-token client ID/secret を設定します。deploy、Managed OAuth、Durable Object migration、reconnect、secret handling の詳細は [`gateway/README.ja.md`](gateway/README.ja.md) を参照してください。
