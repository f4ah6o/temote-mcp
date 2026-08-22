# Cloudflare / Tailscale を使い分ける Ingress / Auth provider architecture を導入する

Status: open
Model: gpt-5.6-sol
Created: 2026-08-23
Updated: 2026-08-23
Branch: main

## 概要

Temote MCP の公開 HTTP endpoint を、Cloudflare 固有構成へ固定せず、Ingress と Auth を独立した provider として扱える構成へ移行する。

Cloudflare Tunnel + Cloudflare Access Managed OAuth は組織利用、既存 IdP、email / policy 管理に有用であり、維持する。

同時に、個人利用や自己完結した構成向けに Tailscale Funnel + Temote self OAuth を第一級の production profile として追加する。

初期段階では任意の provider 組み合わせを正式サポートするのではなく、次の2 profile を production-supported とする。

```text
cloudflare = cloudflare-tunnel + cloudflare-access
tailscale  = tailscale-funnel  + local-oauth
```

## 背景

現在の public HTTP 経路は概ね次の構成になっている。

```text
MCP client
    |
    | OAuth
    v
Cloudflare Access
    |
    | Cf-Access-Jwt-Assertion
    v
Cloudflare Tunnel
    |
    v
127.0.0.1:8791
    |
    v
temote-mcp up
```

この構成では Cloudflare が以下をまとめて担当している。

- public HTTPS ingress
- NAT 越え
- TLS
- OAuth authorization server
- identity / policy
- Access JWT 発行

Temote 側も現在は Cloudflare Access を前提にしており、`AccessAuthenticator`、`http.rs`、`lifecycle.rs`、`doctor`、環境変数、child process lifecycle に Cloudflare 固有知識が入り込んでいる。

一方で、Tailscale Funnel は public HTTPS ingress と NAT 越えを簡潔に提供できるが、Funnel 自体を MCP のユーザー認証境界として扱うべきではない。そのため Tailscale 構成では Temote 自身が OAuth authorization server / bearer authentication を担当する。

Cloudflare と Tailscale は用途が異なり、どちらかを廃止するのではなく使い分けられることを目標とする。

## 目標

- Cloudflare Tunnel + Cloudflare Access Managed OAuth を既存の正式構成として維持する。
- Tailscale Funnel + Temote self OAuth を新しい正式構成として追加する。
- Ingress と Auth を内部設計上は独立した provider とする。
- MCP / session / sandbox / approval の実装を provider に依存させない。
- `temote-mcp up --profile <name>` で構成を選択できるようにする。
- `temote-mcp doctor` が選択 profile に応じて必要な runtime / credentials / endpoint を検査する。
- provider 固有設定を分離し、Cloudflare 固有環境変数を Tailscale 利用者へ要求しない。
- public endpoint から `without_sandbox` を公開しない既存境界を維持する。
- managed session の named-root / symlink escape / approval semantics を変更しない。

## 非目標

- Cloudflare backend を削除すること。
- Tailscale Funnel を authentication boundary とみなすこと。
- 初期実装で Ingress × Auth の全組み合わせを production-supported にすること。
- 汎用 OAuth / OIDC 製品を Temote 内に構築すること。
- 組織向け IdP federation を local OAuth に再実装すること。
- Tailscale Serve の tailnet identity header を public MCP の認証として使うこと。

## 提案アーキテクチャ

```text
                         temote-mcp
                             |
                  +----------+----------+
                  |                     |
               Ingress                 Auth
                  |                     |
          +-------+-------+      +------+-------+
          |               |      |              |
   Cloudflare Tunnel  Tailscale  Cloudflare   Local OAuth
                       Funnel      Access
```

MCP/session/sandbox/approval はこの provider layer より内側に置く。

```text
public HTTPS
    |
    v
IngressProvider
    |
    v
HTTP router
    |
    v
AuthProvider -> Identity
    |
    v
MCP dispatch
    |
    v
SessionSupervisor
    |
    v
sandbox / approval / jobs / Git / bridged MCP
```

## Production profiles

### `cloudflare`

```text
Ingress: Cloudflare Tunnel
Auth:    Cloudflare Access Managed OAuth
```

主な用途:

- 組織利用
- Cloudflare Access policy を利用したい環境
- 既存 IdP / email allowlist / organization policy を利用したい環境
- Cloudflare gateway / Workers / Durable Objects と統合する環境

既存の `Cf-Access-Jwt-Assertion` 検証を維持する。

### `tailscale`

```text
Ingress: Tailscale Funnel
Auth:    Temote local OAuth
```

主な用途:

- 個人所有 host
- Cloudflare account / Tunnel token / Access application を持ちたくない環境
- Tailscale node identity と MagicDNS / `*.ts.net` endpoint を公開経路に使う環境
- 設定を最小化した自己完結構成

Tailscale Funnel は public ingress であり、アクセス可能であること自体を認証済みとはみなさない。MCP request の security boundary は Temote が発行・検証する OAuth bearer token とする。

## CLI

第一案:

```sh
temote-mcp up --profile cloudflare
temote-mcp up --profile tailscale

temote-mcp down
temote-mcp doctor --profile cloudflare
temote-mcp doctor --profile tailscale
```

将来的には config の default profile を許可してよい。

```toml
[default]
profile = "tailscale"

[profile.cloudflare]
ingress = "cloudflare-tunnel"
auth = "cloudflare-access"

[profile.tailscale]
ingress = "tailscale-funnel"
auth = "local-oauth"
```

ただし初期実装では config format の一般化より、既存 CLI / env との migration を優先する。

## Provider contract

実装形は固定しないが、責務として最低限以下を分離する。

```rust
trait IngressProvider {
    async fn start(&self, origin: SocketAddr) -> Result<PublicEndpoint>;
    async fn stop(&self) -> Result<()>;
    async fn doctor(&self) -> Result<DoctorReport>;
}

trait AuthProvider {
    async fn authenticate(&self, request: &Request) -> Result<Identity>;
}
```

重要なのは trait の採否ではなく、以下の依存方向を守ること。

```text
Cloudflare / Tailscale details
            |
            v
provider boundary
            |
            v
Identity / PublicEndpoint
            |
            v
provider-neutral MCP core
```

`mcp.rs`、session dispatch、sandbox、approval は `Cf-Access-Jwt-Assertion` や `tailscale` command を直接参照しない。

## Identity model

Cloudflare Access と local OAuth で得られる claim は一致しないため、内部では provider-neutral identity に正規化する。

例:

```text
Identity
- provider
- subject
- display principal (optional)
- email (optional)
```

Cloudflare の email allowlist は Cloudflare profile の policy として維持する。

local OAuth では email identity を必須としない。単一所有者構成で不要なユーザーDBやメールアドレス保存を導入しない。

## Local OAuth

Tailscale profile では Temote 自身が MCP client 用 OAuth authorization server を提供する。

最低限必要な endpoint / metadata:

```text
/.well-known/oauth-protected-resource
/.well-known/oauth-authorization-server
/authorize
/token
/mcp
```

OAuth authorization code flow を使用し、少なくとも以下を満たす。

- Authorization Code
- PKCE S256 mandatory
- short-lived authorization code
- authorization code single-use
- exact redirect URI validation
- client binding
- resource / audience binding
- short-lived access token
- token の secure random generation または署名付き token
- replay / code reuse rejection
- token endpoint での grant validation
- authorization response の state preservation
- secrets / authorization code / access token を通常ログへ出力しない

既存 MCP client との互換性のために必要な client registration / discovery mechanism は、実装時点の MCP authorization specification に従って決定する。特定の registration mechanism をこの proposal では固定しない。

## Local owner approval

local OAuth は汎用ユーザー管理画面を持つのではなく、Temote の既存 out-of-band approval model と整合させる。

候補:

```text
MCP client
    |
    | GET /authorize
    v
temote-mcp
    |
    | local terminal approval
    v
owner approves client / scope / redirect
    |
    v
authorization code
    |
    v
MCP client
```

初回 authorization を local terminal で所有者が明示承認することで、外部 IdP、password database、email database を必須にしない。

authorization approval と、各 host/network-sensitive tool の runtime approval は別の security decision として維持する。

## Ingress lifecycle

### Cloudflare

既存と同様、`temote-mcp up --profile cloudflare` が origin と `cloudflared` lifecycle を管理できる。

### Tailscale

`temote-mcp up --profile tailscale` は origin と Tailscale Funnel lifecycle を一体として扱う。

期待する概念上の動作:

```text
temote-mcp serve on 127.0.0.1:8791
        +
tailscale funnel -> 127.0.0.1:8791
```

`down` は自分が開始した ingress のみを停止し、他用途で動作している Tailscale daemon 自体は停止しない。

## Public URL

provider が `PublicEndpoint` を返し、HTTP / OAuth metadata がその canonical URL を利用する。

Cloudflare profile では既存 `TEMOTE_MCP_PUBLIC_URL` を利用可能とする。

Tailscale profile では `*.ts.net` の HTTPS origin を利用し、利用可能なら runtime から安全に導出する。導出不能・曖昧な場合は明示設定へ fallback する。

public URL は引き続き以下を満たす。

- HTTPS origin
- username / password なし
- query / fragment なし
- metadata と `/mcp` の canonical origin が一致

## Configuration isolation

Cloudflare profile だけが以下を必要とする。

```text
TEMOTE_MCP_ACCESS_TEAM_DOMAIN
TEMOTE_MCP_ACCESS_AUDIENCE
TEMOTE_MCP_ACCESS_ALLOWED_EMAILS
Tunnel token / Cloudflare diagnostics credentials
```

Tailscale profile はこれらを要求しない。

Tailscale profile だけが必要とする設定は最小化する。Tailscale node / daemon から安全に取得可能な値を重複して secret file に保存しない。

共通設定例:

```text
TEMOTE_MCP_ROOTS
listen address
runtime directory
```

## Doctor

`doctor` は provider-aware にする。

### cloudflare checks

- `cloudflared` availability
- tunnel token file
- public URL
- Access team domain / audience / allowlist
- optional Cloudflare API diagnostics

### tailscale checks

- `tailscale` CLI availability
- tailscaled connectivity
- Funnel availability / capability
- node hostname / expected `*.ts.net` HTTPS endpoint
- local OAuth key / state storage permissions if persistent state is required
- public metadata endpoint consistency

共通 checks は provider-neutral に維持する。

## セキュリティ境界

### 共通

- public HTTP authentication は session sandbox の代替ではない。
- public endpoint から `without_sandbox` を公開しない。
- managed `session_start` は named-root-relative path のみを受け付ける。
- absolute path、unknown root、traversal、symlink escape を拒否する。
- host/network-sensitive operation の local approval を維持する。
- OAuth / Access authentication 後も yolo session を暗黙作成しない。

### Cloudflare profile

- Cloudflare Access JWT の署名、issuer、audience、expiry を Temote origin でも検証する既存 defense-in-depth を維持する。
- configured email allowlist を維持する。

### Tailscale profile

- Funnel exposure を identity proof として扱わない。
- OAuth bearer token が public MCP authentication boundary になる。
- authorization code / token を filesystem に保存する場合は private permissions と atomic write を要求する。
- long-lived static bearer token 一個だけで OAuth を代替しない。
- localhost callback を許可する場合も redirect validation を緩めない。

## Compatibility

既存 Cloudflare 利用者の migration を壊さない。

理想的には現在の設定だけで `cloudflare` profile として解釈できる migration path を用意する。

```text
existing config
    |
    v
implicit cloudflare profile
```

新規利用者には profile を明示する。

将来的に default を変更する場合は別 issue / release decision とする。

## 実装フェーズ案

### Phase 1: provider boundary

- [ ] 現在の Cloudflare 固有 ingress lifecycle を `IngressProvider` 相当の境界へ移す。
- [ ] `AccessAuthenticator` を `AuthProvider` 相当の境界へ移す。
- [ ] provider-neutral `Identity` / `PublicEndpoint` を定義する。
- [ ] Cloudflare profile の既存 behavior を regression test で固定する。
- [ ] `up/down/doctor` を profile-aware にする。

### Phase 2: local OAuth

- [ ] OAuth authorization server metadata を実装する。
- [ ] OAuth protected resource metadata を実装する。
- [ ] authorization code + PKCE S256 を実装する。
- [ ] access token issuance / validation を実装する。
- [ ] local owner approval を実装する。
- [ ] redirect / resource / client binding の negative tests を追加する。
- [ ] token / code replay tests を追加する。

### Phase 3: Tailscale Funnel

- [ ] Tailscale ingress provider を実装する。
- [ ] `temote-mcp up --profile tailscale` を実装する。
- [ ] `temote-mcp down` で Temote が所有する Funnel lifecycle のみ終了する。
- [ ] `doctor --profile tailscale` を実装する。
- [ ] canonical `*.ts.net` public URL の解決を実装する。

### Phase 4: live acceptance

- [ ] Cloudflare profile の既存 MCP client 接続を再検証する。
- [ ] Tailscale profile で外部 MCP client から OAuth discovery / authorization / `/mcp` を検証する。
- [ ] 両 profile で managed session start / stop を検証する。
- [ ] 両 profile で approval / sandbox boundary が同一であることを検証する。

## 受け入れ条件

- [ ] `temote-mcp up --profile cloudflare` が既存 Cloudflare Tunnel + Access behavior を維持する。
- [ ] `temote-mcp up --profile tailscale` が Tailscale Funnel + local OAuth で public MCP endpoint を公開する。
- [ ] Tailscale profile は Cloudflare account、Tunnel token、Access application を要求しない。
- [ ] Cloudflare profile は local OAuth state を要求しない。
- [ ] unauthenticated `/mcp` request は両 profile で拒否される。
- [ ] Cloudflare profile は invalid Access JWT を拒否する。
- [ ] Tailscale profile は invalid / expired / wrong-resource OAuth token を拒否する。
- [ ] local OAuth は PKCE downgrade、code replay、redirect mismatch を拒否する。
- [ ] provider-neutral MCP dispatch に Cloudflare / Tailscale 固有 header や process control が漏れない。
- [ ] `session_start` / `session_stop` / named roots の security semantics が両 profile で同一である。
- [ ] `without_sandbox` が public tool list に現れない。
- [ ] `doctor` が選択 profile に不要な provider の dependency / secret を failure にしない。
- [ ] Cloudflare と Tailscale の live acceptance evidence を残す。
- [ ] README / `docs/public-http*.md` / development docs を profile architecture に更新する。

## テスト方針

- Cloudflare provider regression unit tests
- provider-neutral auth dispatch tests
- local OAuth state-machine unit tests
- authorization code single-use property tests
- redirect URI exact-match property tests
- resource / audience binding property tests
- token expiry / replay property tests
- profile-specific environment validation tests
- lifecycle ownership tests
- `doctor` provider matrix tests
- Cloudflare live acceptance
- Tailscale Funnel live acceptance
- external MCP client OAuth discovery / authorization / tool call acceptance

既存の `noprop` PBT suite を security boundary の不変条件へ積極的に適用する。

## 設計上の判断

### Ingress と Auth を分離する

`cloudflare` / `tailscale` を巨大な条件分岐として MCP core に入れない。公開経路と認証方式は概念上別責務として扱う。

### ただし support matrix は絞る

内部的に provider を直交させても、初期 production support は以下だけとする。

```text
cloudflare-tunnel + cloudflare-access
 tailscale-funnel + local-oauth
```

未検証の組み合わせを CLI が偶然作れることと、正式サポートは区別する。

### Cloudflare を legacy 扱いしない

Cloudflare Access の managed identity / policy は組織利用で明確な利点があるため、Tailscale 導入後も第一級 backend として維持する。

### Tailscale を認証そのものにしない

Tailscale Serve と Funnel の semantics を混同しない。public MCP 用 Tailscale profile では Funnel は ingress、local OAuth は auth と明確に分離する。

## リスク

- self OAuth 実装は security-sensitive であり、単なる tunnel backend 追加より大きな変更になる。
- MCP authorization specification と主要 client の compatibility を実装時点で再確認する必要がある。
- Tailscale CLI / Funnel の platform 差や lifecycle ownership を誤ると、Temote 外の既存 Serve / Funnel 設定へ干渉する可能性がある。
- Cloudflare 固有コードの抽象化時に既存 Access defense-in-depth を弱めない必要がある。
- provider abstraction を先に一般化しすぎると不要な complexity を増やすため、2つの実 profile から必要な contract のみ抽出する。

## 変更履歴

`CHANGES.md` impact: no（proposal only）
