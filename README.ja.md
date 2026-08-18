# local-mcp

[English](README.md)

`local-mcp` は [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) から派生したプロジェクトです。安全なリモートアクセス、ゲートウェイ運用、MCP ブリッジを中心に拡張しています。GitHub 上では fork として紐づいていないため、ここで出自を明記しています。

名前の着想には、[@mr_konn が「remote」の対義語として提唱した「テモート」](https://x.com/mr_konn/status/1318116448519114752?s=46) もあります。元の投稿では英字表記が示されていないため、このリポジトリでは **Temote** と表記します。例示用ホスト名 `temotemcp.example.com` もこの表記に合わせています。upstream と同じく、この命名の出典もプロジェクトの由来として記録します。

local-mcp は、手元のファイル操作とサンドボックス内のコマンド実行を MCP ツールとして公開します。Web 検索や任意のネットワークリクエスト機能はありません。通常モードのコマンドはネットワークから隔離されます。Linux では固定した Codex sandbox stack、macOS では local-mcp 独自の Seatbelt バックエンドを使います。

公開 HTTP エンドポイントは、必要なときだけ Cloudflare Tunnel を起動する構成を想定しています。

    ChatGPT Plus
        | Managed OAuth
        v
    Cloudflare Access -- Cloudflare Tunnel -- 127.0.0.1:8791
                                                   |
                                                   v
                                            local-mcp serve

## ビルド

    cargo build --release
    cargo install --path . --locked

通常のビルドには公開 HTTP と `gateway-agent` が含まれます。`doctor`、`start`、`mcp` だけを含むローカル専用バイナリが必要なら、既定の `network` feature を無効にします。

    cargo build --release --no-default-features --locked

Linux では local-mcp と `codex-linux-sandbox` を同じディレクトリに置き、`bwrap` を PATH から実行できるようにしてください。macOS はシステムの Seatbelt を使います。Windows ネイティブには対応していません。

インストール時は `--locked` を付けてください。Linux 用の Codex sandbox 依存関係は Git revision に固定されています。commit 済みの lockfile を使わないと、互換性のない prerelease 版の `rama` や `starlark` が選ばれることがあります。macOS では Codex sandbox crate 自体を解決しません。

セッションを開始する前、または ChatGPT から接続する前に診断を実行します。

    local-mcp doctor

Linux の `doctor` は `codex-linux-sandbox`、`bubblewrap`、user namespace、隔離した network namespace、実際のサンドボックス実行、shell 用の実行環境を確認します。必須項目に失敗すると終了コードは non-zero です。

## リリース番号

リリース番号は [`f4ah6o/calver-action`](https://github.com/f4ah6o/calver-action) で割り当てる CalVer `YYYY.MM.PATCH` です。日付の基準は `Asia/Tokyo`。`main` 上のリリース対象 commit に `latest` tag を移すとリリースが始まります。

    git tag -f latest <commit-to-release>
    git push -f origin latest

`.github/workflows/release.yaml` は次の prefixless CalVer tag を採番し、リリース専用 commit で `Cargo.toml` と `Cargo.lock` を更新します。その後、通常ビルドと local-only ビルドを検証し、immutable tag を push します。この commit は `main` には戻しません。

## ローカルセッション

セッションは、そのセッションから触らせたいプロジェクトのディレクトリで開始します。

    cd ~/src/local-mcp
    local-mcp start local-mcp

    cd ~/src/shuttle-rs
    local-mcp start shuttle-rs

制限を外して実行する場合は `--yolo` を付けます。

    local-mcp start local-mcp --yolo

YOLO mode では local-mcp の承認プロンプトとパス制限がなくなり、コマンドは local-mcp を実行しているユーザー権限でホスト上に直接実行されます。ファイル、環境変数、プロセス、ネットワークもその権限を引き継ぎます。危険なモードなので、用途を限定してください。実行中は `/permission ask` で通常モード、`/permission yolo` で YOLO mode に切り替えられます。

`session_list` を除くすべてのツール呼び出しには `session_id` が必要です。`session_list` で起動中のセッションを探し、`session_info` で作業ディレクトリと許可済みのルートを確認できます。

別のディレクトリも許可する場合は、そのセッションを起動したローカル端末で設定します。

    /permission allow ../another-project
    /permission revoke ../another-project
    /permission list
    /permission status

ChatGPT から Git を操作するときは、ローカル commit に `git_add` と `git_commit`、リモート同期に `git_fetch`、`git_pull`、`git_push` を使います。リモート操作は通常、ホスト側の承認が必要です。任意 URL/refspec や force push は受け付けず、`git_pull` は fast-forward-only です。

## 公開 HTTP エンドポイント

`.env.example` をリポジトリの外へコピーし、Cloudflare Access の設定値を入れます。Tunnel token は認証情報なので、ファイルの mode は 0600 にしてください。

    install -d -m 700 ~/.config/local-mcp
    cp .env.example ~/.config/local-mcp/public.env
    chmod 600 ~/.config/local-mcp/public.env
    vi ~/.config/local-mcp/public.env

サービスが必要な間だけ、別々の端末で origin と Tunnel を起動します。

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

`justfile` から同じ操作を行えます。

    just build
    just doctor
    just env-check
    just up
    just down
    just serve
    just tunnel
    just start local-mcp
    just start shuttle-rs ~/src/shuttle-rs

公開 URL の例は次のとおりです。

    https://temotemcp.example.com/mcp

Cloudflare 側では次の4点を設定します。

1. remotely managed Tunnel を作り、`temotemcp.example.com` を `http://127.0.0.1:8791` へ向けます。
2. ホスト全体を保護する **self-hosted Cloudflare Access application** を作ります。`/mcp` だけに制限しないでください。Managed OAuth の discovery はホスト直下の `/.well-known/` を使います。
3. 接続を許すメールアカウントだけを Allow policy に入れます。この公開エンドポイントには Bypass や Service Auth policy を追加しません。
4. self-hosted application の Advanced settings で Managed OAuth を有効にし、dynamic client registration、access token の寿命、grant session の期間を設定します。ChatGPT の redirect URI は次の2つです。

       https://chatgpt.com/connector/oauth/*
       https://chatgpt.com/connector_platform_oauth_redirect

`AI controls > MCP servers` に作る portal registration は別用途です。ChatGPT が `temotemcp.example.com` に直接接続する構成を保護するのは、上記の self-hosted application です。

Managed OAuth の処理は Cloudflare Access が担当します。Rust 側では `Cf-Access-Jwt-Assertion` の署名、issuer、audience、有効期限、subject、許可メールアドレスを検証します。`LOCAL_MCP_ACCESS_AUDIENCE` には `temotemcp.example.com` を保護している self-hosted application の AUD を設定します。

接続前は、次の probe で Access が origin より手前で応答していることを確認できます。

    curl -i -X POST https://temotemcp.example.com/mcp \
      -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'
    curl -i https://temotemcp.example.com/.well-known/oauth-authorization-server
    curl -i https://temotemcp.example.com/.well-known/oauth-protected-resource

未認証の MCP request は Cloudflare の `WWW-Authenticate` header 付き `401`、2つの discovery endpoint は JSON metadata 付き `200` が正常です。`530` なら on-demand Tunnel か local origin が止まっています。

## 制約と安全境界

- 公開リクエストには有効な Cloudflare Access assertion が必要です。
- `session_list` 以外の公開ツールでは、起動中の `session_id` を指定します。
- 通常モードでは、ファイルパス、symlink の参照先、コマンドの cwd を許可済みルート内に制限します。
- 通常モードのサンドボックスコマンドはネットワークへ接続できません。
- `--yolo` ではパス制限とサンドボックスを外し、ホストユーザーの権限でコマンドを実行します。
- 通常のサンドボックスコマンドから `.git` は書き換えられません。Git metadata の変更には専用の Git ツールを使います。
- リモート Git 操作も専用ツール経由です。`git_pull` は fast-forward-only、`git_push` には force option がありません。
- 1セッションで同時に実行できるサンドボックス job は4件までです。
- stdout と stderr の合計は1コマンドあたり 1 MiB までです。
- background job は2時間で停止します。セッションが終了した場合も停止します。
- runtime audit にはツール名、セッション、status、実行時間を記録します。認証メール/subject やコマンド引数、出力内容は永続ログに残しません。
- secret-file denylist はありません。`/home` のような広いディレクトリを許可せず、必要な場所だけを追加してください。

## ローカル stdio

MCP client が local-mcp の process を直接起動する場合は、HTTP ではなく stdio mode を使えます。

    local-mcp mcp

Cloudflare Access を使う公開 HTTP エンドポイントとは独立したモードです。

## 1Password Environments MCP

local-mcp は公式の 1Password Environments MCP server をブリッジできます。通常は macOS なら `/Applications/1Password.app/Contents/MacOS/1password-mcp`、Linux なら `/opt/1Password/1password-mcp` を使います。別の場所にインストールした場合だけ `LOCAL_MCP_ONEPASSWORD_MCP` を指定します。

公開エンドポイントとローカルエンドポイントには、次の3つのブリッジツールがあります。

- `onepassword_mcp_discover` は child server が公開している resource と tool schema を列挙します。
- `onepassword_mcp_read_resource` は child server が公開した resource だけを読み取ります。
- `onepassword_mcp_call` は persistent stdio connection 経由で child tool を呼び出します。

通常セッションでは、書き込みを伴う child tool も local-mcp の承認対象です。secret の扱いは公式 1Password MCP server に任せており、local-mcp 独自の secret-reading API は追加していません。

### Service-account mode

無人実行で secret を注入する場合は、`local-mcp start` に service-account token を渡します。

    OP_SERVICE_ACCOUNT_TOKEN='<service-account-token>' local-mcp start my-project

service-account token は session process が保持し、session JSON metadata には書き込みません。

このモードでは `onepassword_service_account_status` で token の有無と `op whoami` の成否を値を返さずに確認でき、`onepassword_service_account_run` で `op run` 経由のコマンドを実行できます。secret reference には `op://...` を使い、平文値は拒否します。

## kintone MCP Server

local-mcp は公式の [`@kintone/mcp-server`](https://github.com/kintone/mcp-server) もブリッジできます。先にホストへ CLI をインストールしてください。

    npm install -g @kintone/mcp-server

credential は `local-mcp serve` や `gateway-agent` ではなく、**`local-mcp start` process** に渡します。

    KINTONE_BASE_URL='https://example.cybozu.com' \
    KINTONE_API_TOKEN='<api-token>' \
    local-mcp start my-project

username/password 認証も使えます。

    KINTONE_BASE_URL='https://example.cybozu.com' \
    KINTONE_USERNAME='<username>' \
    KINTONE_PASSWORD='<password>' \
    local-mcp start my-project

公開/ローカルの各エンドポイントは `kintone_mcp_status`、`kintone_mcp_discover`、`kintone_mcp_call` を提供します。status から credential の値や tenant URL は返しません。

## Serverless multi-host gateway

[`gateway/`](gateway/) は任意で使える Cloudflare Workers ベースのゲートウェイです。1つの MCP エンドポイントを公開し、`session_id` ごとに Durable Object を介して接続先ホストを選びます。Mac、Linux、Windows/WSL2 側の `local-mcp gateway-agent` は outbound-only の HTTPS long poll だけを行うため、ホストごとの inbound port や Tunnel は不要です。

代表的な起動例です。

    # Mac
    local-mcp start mac-main
    local-mcp gateway-agent --session-id mac-main

    # Windows interim path, inside WSL2
    local-mcp start windows-wsl2-main
    local-mcp gateway-agent --session-id windows-wsl2-main --platform wsl2

`LOCAL_MCP_GATEWAY_URL`、`LOCAL_MCP_GATEWAY_HOST_TOKEN` と、必要なら Cloudflare Access の service-token client ID/secret を設定します。デプロイ、Managed OAuth、Durable Object migration、再接続、secret の扱いは [`gateway/README.ja.md`](gateway/README.ja.md) にまとめています。
