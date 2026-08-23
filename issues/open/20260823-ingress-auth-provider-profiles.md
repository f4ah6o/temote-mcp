# Cloudflare / Tailscale / OpenAI Secure MCP Tunnel を使い分ける connection provider architecture を導入する

Status: open
Model: gpt-5.6-sol
Created: 2026-08-23
Updated: 2026-08-23
Branch: main

## 概要

Temote MCP の remote connection を Cloudflare 固有構成へ固定せず、Ingress / Auth / Connection の責務を分離し、用途に応じて複数 profile を選べる構成へ移行する。

次の3 profile を第一級候補として扱う。

```text
cloudflare = Cloudflare Tunnel + Cloudflare Access Managed OAuth
tailscale  = Tailscale Funnel + Temote local OAuth
openai     = OpenAI Secure MCP Tunnel
```

Cloudflare と Tailscale は public HTTPS endpoint を作る general-purpose ingress である。一方 OpenAI Secure MCP Tunnel は、private network / on-premises / developer machine 上の MCP server を public Internet に公開せず supported OpenAI products へ接続する OpenAI-specific private connection であり、性質が異なる。

したがって内部では provider abstraction を共有しつつ、すべてを無理に同じ `IngressProvider + AuthProvider` の形へ押し込めない。

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

Temote 側も `AccessAuthenticator`、`http.rs`、`lifecycle.rs`、`doctor`、環境変数、child process lifecycle に Cloudflare 固有知識を持っている。

一方、用途別に次の選択肢がある。

### Cloudflare

Cloudflare Tunnel + Access Managed OAuth は、組織利用、IdP、email / policy 管理、Cloudflare Workers / Durable Objects との統合に有用である。

### Tailscale

Tailscale Funnel は public HTTPS ingress と NAT 越えを簡潔に提供できる。Funnel 自体を MCP user authentication boundary とみなさず、Temote local OAuth と組み合わせる。

### OpenAI Secure MCP Tunnel

OpenAI は、private network / on-premises / developer machine 上で動く MCP server を public Internet に露出せず supported OpenAI products へ接続する用途として Secure MCP Tunnel を提供している。

これは Cloudflare Tunnel / Tailscale Funnel の代替というより、OpenAI product へ限定した private connection path である。

## 目標

- Cloudflare Tunnel + Cloudflare Access Managed OAuth を既存の正式構成として維持する。
- Tailscale Funnel + Temote local OAuth を正式構成として追加する。
- OpenAI Secure MCP Tunnel を OpenAI-specific private connection profile として追加する。
- MCP / session / sandbox / approval の実装を connection provider に依存させない。
- `temote-mcp up --profile <name>` で connection profile を選択できるようにする。
- `temote-mcp doctor --profile <name>` が profile ごとの runtime / credentials / endpoint を検査する。
- provider 固有設定を分離し、選択していない provider の secret / dependency を要求しない。
- public endpoint から `without_sandbox` を公開しない既存境界を維持する。
- managed session の named-root / symlink escape / approval semantics を変更しない。
- OpenAI Secure MCP Tunnel 利用時も Temote の local approval / sandbox を security boundary として維持する。

## 非目標

- Cloudflare backend を削除すること。
- Tailscale Funnel を authentication boundary とみなすこと。
- OpenAI Secure MCP Tunnel を general-purpose public endpoint として扱うこと。
- Secure MCP Tunnel の OpenAI control plane を Temote が再実装すること。
- 初期実装で provider の全組み合わせを production-supported にすること。
- 汎用 OAuth / OIDC 製品を Temote 内に構築すること。
- 組織向け IdP federation を local OAuth に再実装すること。

## Connection model

単純な `Ingress × Auth` だけでは Secure MCP Tunnel を自然に表現できないため、上位概念として `ConnectionProfile` を置く。

```text
                         temote-mcp
                             |
                             v
                     ConnectionProfile
                             |
          +------------------+------------------+
          |                  |                  |
      cloudflare         tailscale            openai
          |                  |                  |
          v                  v                  v
  public HTTPS        public HTTPS       private OpenAI
  Cloudflare Tunnel   Tailscale Funnel   Secure MCP Tunnel
          |                  |                  |
  Cloudflare Access   Temote local OAuth OpenAI tunnel path
```

public ingress profiles の内部では Ingress / Auth を引き続き独立責務として扱う。

```text
ConnectionProfile
    |
    +--> PublicProfile
    |       |
    |       +--> IngressProvider
    |       +--> AuthProvider
    |
    +--> PrivateTunnelProfile
            |
            +--> TunnelProvider
```

実装上 trait を必須とはしない。重要なのは provider-neutral MCP core への依存方向である。

```text
Cloudflare / Tailscale / OpenAI details
                  |
                  v
          connection boundary
                  |
                  v
       ConnectionIdentity / Endpoint
                  |
                  v
        provider-neutral MCP core
                  |
                  v
    SessionSupervisor / sandbox / approval
```

## Production profiles

### `cloudflare`

```text
Connection: public HTTPS
Ingress:    Cloudflare Tunnel
Auth:       Cloudflare Access Managed OAuth
```

主な用途:

- 組織利用
- Cloudflare Access policy
- 既存 IdP / email allowlist / organization policy
- custom domain
- Cloudflare Workers / Durable Objects gateway
- OpenAI 以外の MCP client からも同じ endpoint を使う場合

既存の `Cf-Access-Jwt-Assertion` 検証を維持する。

### `tailscale`

```text
Connection: public HTTPS
Ingress:    Tailscale Funnel
Auth:       Temote local OAuth
```

主な用途:

- 個人所有 host
- Cloudflare account / Tunnel token / Access application を持ちたくない環境
- Tailscale node identity / MagicDNS / `*.ts.net` を利用する環境
- OpenAI 以外の MCP client にも公開する可能性がある環境
- 設定を最小化した自己完結構成

Tailscale Funnel は public ingress である。アクセス可能であること自体を authentication とみなさず、MCP authentication boundary は Temote local OAuth bearer token とする。

### `openai`

```text
Connection: private OpenAI tunnel
Transport:  OpenAI Secure MCP Tunnel
Exposure:   no public Internet endpoint required
```

主な用途:

- ChatGPT など Secure MCP Tunnel 対応 OpenAI product から Temote を使う
- local developer machine
- private network
- on-premises host
- public domain / public ingress を作りたくない環境
- OAuth server を public Internet に公開したくない環境

OpenAI 公式ドキュメント上、Secure MCP Tunnel は local/private MCP server を public Internet に露出せず supported OpenAI products へ接続するための経路である。

この profile は Cloudflare / Tailscale より attack surface を小さくできる可能性があるため、OpenAI-only 利用では有力な default candidate とする。ただし product availability / workspace entitlement / tunnel protocol は OpenAI 側仕様に依存するため、実装時点の公式仕様を再確認する。

## Support matrix

初期 production support は次の3構成に限定する。

| Profile | Public Internet exposure | Client scope | Auth / trust boundary |
| --- | --- | --- | --- |
| `cloudflare` | yes, Access protected | general MCP clients | Cloudflare Access + origin JWT validation |
| `tailscale` | yes, OAuth protected | general MCP clients | Temote local OAuth |
| `openai` | no public endpoint required | supported OpenAI products | OpenAI Secure MCP Tunnel connection + Temote local security boundaries |

未検証の provider 組み合わせを偶然構成できることと、production-supported であることは区別する。

## CLI

第一案:

```sh
temote-mcp up --profile cloudflare
temote-mcp up --profile tailscale
temote-mcp up --profile openai

temote-mcp down

temote-mcp doctor --profile cloudflare
temote-mcp doctor --profile tailscale
temote-mcp doctor --profile openai
```

config の default profile は将来的に許可する。

```toml
[default]
profile = "openai"

[profile.cloudflare]
connection = "public"
ingress = "cloudflare-tunnel"
auth = "cloudflare-access"

[profile.tailscale]
connection = "public"
ingress = "tailscale-funnel"
auth = "local-oauth"

[profile.openai]
connection = "openai-secure-mcp-tunnel"
```

default を実際に `openai` へ変更するかは live compatibility と plan/workspace availability を確認した後に別 decision とする。

## Provider contracts

概念上の contract:

```rust
trait ConnectionProvider {
    async fn start(&self, origin: SocketAddr) -> Result<ConnectionEndpoint>;
    async fn stop(&self) -> Result<()>;
    async fn doctor(&self) -> Result<DoctorReport>;
}

trait IngressProvider {
    async fn start(&self, origin: SocketAddr) -> Result<PublicEndpoint>;
    async fn stop(&self) -> Result<()>;
    async fn doctor(&self) -> Result<DoctorReport>;
}

trait AuthProvider {
    async fn authenticate(&self, request: &Request) -> Result<Identity>;
}
```

`ConnectionProvider` は public endpoint を必ず返すとは限らない。

例:

```text
ConnectionEndpoint
- PublicHttps(origin)
- OpenAiSecureTunnel(tunnel identity/reference)
```

OpenAI Secure MCP Tunnel の concrete API / identifier / credential model は OpenAI 公式仕様に合わせ、推測で固定しない。

## Identity model

Cloudflare Access、local OAuth、OpenAI Secure MCP Tunnel で得られる identity semantics は一致しないため、内部では provider-neutral identity/context に正規化する。

例:

```text
ConnectionIdentity
- provider
- subject (optional)
- display principal (optional)
- email (optional)
- connection_id (optional)
```

Cloudflare の email allowlist は Cloudflare profile の policy として維持する。

local OAuth では email identity を必須としない。

OpenAI profile では OpenAI tunnel が提供する identity/trust information のみを使用し、存在しない claim を推測して生成しない。

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
- token endpoint grant validation
- state preservation
- secrets / code / token を通常ログへ出力しない

client registration / discovery mechanism は実装時点の MCP authorization specification に従う。

## Local owner approval

local OAuth authorization と Temote runtime approval は別 security decision とする。

```text
MCP client
    |
    | authorize
    v
Temote local OAuth
    |
    | owner approval
    v
OAuth grant

later:

tools/call
    |
    v
Temote sandbox / host-sensitive approval
```

OpenAI Secure MCP Tunnel profile でも host/network-sensitive tool の Temote local approval は維持する。

## Lifecycle

### Cloudflare

`temote-mcp up --profile cloudflare` が origin と `cloudflared` child lifecycle を管理する。

### Tailscale

`temote-mcp up --profile tailscale` が origin と Temote が開始した Funnel lifecycle を管理する。

```text
temote-mcp serve on 127.0.0.1:8791
        +
tailscale funnel -> 127.0.0.1:8791
```

`down` は tailscaled daemon 自体や Temote 外で管理されている Serve/Funnel を停止しない。

### OpenAI Secure MCP Tunnel

`temote-mcp up --profile openai` は local MCP HTTP origin と Secure MCP Tunnel client/process/session を一体で管理する。

概念上:

```text
supported OpenAI product
        |
        v
OpenAI Secure MCP Tunnel service
        ^
        |
outbound/private tunnel connection
        |
        v
local Temote MCP endpoint
```

Temote host に public inbound port を要求しない。

具体的な tunnel client command、credential、registration lifecycle は実装時点の OpenAI 公式仕様に従う。外部 command を spawn する方式か library/API integration かもこの proposal では固定しない。

## Endpoint model

Cloudflare/Tailscale は canonical public HTTPS URL を持つ。

OpenAI profile は public URL を必須としない。

したがって、既存 `TEMOTE_MCP_PUBLIC_URL` を global required value とせず public profiles に限定する。

```text
cloudflare -> PublicEndpoint(https://...)
tailscale  -> PublicEndpoint(https://....ts.net)
openai     -> SecureTunnelEndpoint(...)
```

MCP OAuth metadata が必要な profile だけ canonical public origin を要求する。

## Configuration isolation

### Cloudflare only

```text
TEMOTE_MCP_ACCESS_TEAM_DOMAIN
TEMOTE_MCP_ACCESS_AUDIENCE
TEMOTE_MCP_ACCESS_ALLOWED_EMAILS
Tunnel token
Cloudflare diagnostics credentials
```

### Tailscale only

- Tailscale CLI / daemon availability
- Funnel capability
- local OAuth state / key material if persistence is required

Cloudflare secrets を要求しない。

### OpenAI only

- Secure MCP Tunnel feature availability
- OpenAI-side tunnel registration / authorization required by the official implementation
- local tunnel client state/credential required by the official implementation

Cloudflare / Tailscale dependency を要求しない。

OpenAI credential の具体名・保存形式は公式仕様確認前に固定しない。

### Common

```text
TEMOTE_MCP_ROOTS
listen address
runtime directory
session / sandbox configuration
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
- expected `*.ts.net` endpoint
- local OAuth state permissions
- public OAuth metadata consistency

### openai checks

- Secure MCP Tunnel client/runtime availability
- required OpenAI tunnel configuration / authorization
- local MCP endpoint reachability from tunnel client
- no accidental public bind requirement
- tunnel connection health when observable
- OpenAI-specific profile does not require Cloudflare/Tailscale dependencies

OpenAI側 control plane availability が原因の failure と Temote local origin failure を区別して報告する。

## セキュリティ境界

### 共通

- remote connection security は session sandbox の代替ではない。
- public endpoint / tunnel endpoint から `without_sandbox` を公開しない。
- managed `session_start` は named-root-relative path のみを受け付ける。
- absolute path、unknown root、traversal、symlink escape を拒否する。
- host/network-sensitive operation の local approval を維持する。
- authenticated connection から yolo session を暗黙作成しない。

### Cloudflare profile

- Access JWT の署名、issuer、audience、expiry を Temote origin でも検証する existing defense-in-depth を維持する。
- configured email allowlist を維持する。

### Tailscale profile

- Funnel exposure を identity proof として扱わない。
- OAuth bearer token が public MCP authentication boundary になる。
- PKCE / redirect / resource binding を厳格に検証する。
- long-lived static bearer token 一個だけで OAuth を代替しない。

### OpenAI profile

- local MCP origin を public interface に bind する必要を作らない。
- Secure MCP Tunnel 接続成功を yolo / sandbox bypass の根拠にしない。
- OpenAI product 側 confirmation と Temote local approval を混同しない。
- tunnel credential / registration secret がある場合はログへ出力しない。
- OpenAI-specific tunnel transport を general Internet ingress として再公開しない。

## Compatibility

既存 Cloudflare 利用者を壊さない。

既存設定のみが存在する場合は implicit `cloudflare` profile として解釈できる migration path を用意する。

```text
existing Cloudflare config
        |
        v
implicit cloudflare profile
```

新規利用者には profile 明示を基本とする。

OpenAI Secure MCP Tunnel availability は OpenAI product / workspace / plan に依存し得るため、利用不能な環境では明確な doctor error を返し、Cloudflare または Tailscale へ暗黙 fallback しない。

## 実装フェーズ案

### Phase 1: connection boundary

- [x] `ConnectionProfile` を導入する。
- [x] 現在の Cloudflare lifecycle を provider boundary へ移す。
- [x] `AccessAuthenticator` を provider-specific auth boundary へ移す。
- [x] provider-neutral `ConnectionIdentity` / endpoint model を定義する。
- [x] Cloudflare existing behavior を regression test で固定する。
- [x] `up/down/doctor` を profile-aware にする。

### Phase 2: Tailscale local OAuth

- [x] OAuth authorization server metadata を実装する。
- [x] OAuth protected resource metadata を実装する。
- [x] authorization code + PKCE S256 を実装する。
- [x] access token issuance / validation を実装する。
- [x] local owner approval を実装する。
- [x] redirect / resource / client binding negative tests を追加する。
- [x] token / code replay tests を追加する。

### Phase 3: Tailscale Funnel

- [x] Tailscale connection provider を実装する。
- [x] `temote-mcp up --profile tailscale` を実装する。
- [x] Temote-owned Funnel lifecycle のみ stop する。
- [x] `doctor --profile tailscale` を実装する。
- [x] canonical `*.ts.net` public URL 解決を実装する。

### Phase 4: OpenAI Secure MCP Tunnel

- [x] 実装時点の OpenAI official Secure MCP Tunnel documentation / protocol / client tooling を確認する。
- [x] OpenAI Secure MCP Tunnel connection provider を実装する。
- [x] `temote-mcp up --profile openai` を実装する。
- [x] tunnel registration / credential / lifecycle ownership を安全に管理する。
- [x] `temote-mcp down` が Temote-owned tunnel connection のみ停止する。
- [x] `doctor --profile openai` を実装する。
- [x] `temote-mcp openai setup` から Tunnel Management API で tunnel record を bootstrap する。
- [x] `CONTROL_PLANE_TUNNEL_ID` だけを private local config に保存し admin/runtime API key を永続化しない。
- [x] public bind / public URL を要求しないことを regression test で固定する。

### Phase 5: live acceptance

- [ ] Cloudflare profile の既存 MCP client 接続を再検証する。
- [x] Tailscale profile で external MCP client の OAuth discovery / authorization / `/mcp` を検証する。
- [ ] OpenAI Secure MCP Tunnel 経由で supported OpenAI product から Temote を live 検証する。
- [ ] 3 profile で managed session start / stop semantics を検証する。
- [ ] 3 profile で approval / sandbox boundary が同一であることを検証する。

## 受け入れ条件

- [ ] `temote-mcp up --profile cloudflare` が existing Cloudflare Tunnel + Access behavior を維持する。
- [x] `temote-mcp up --profile tailscale` が Tailscale Funnel + local OAuth で public MCP endpoint を提供する。
- [ ] `temote-mcp up --profile openai` が OpenAI Secure MCP Tunnel を利用し public Internet endpoint を要求せず supported OpenAI product と接続できる。
- [x] Tailscale profile は Cloudflare account / Tunnel token / Access application を要求しない。
- [x] OpenAI profile は Cloudflare / Tailscale dependency を要求しない。
- [x] Cloudflare profile は local OAuth state を要求しない。
- [x] unauthenticated public `/mcp` request は Cloudflare/Tailscale profile で拒否される。
- [x] Cloudflare profile は invalid Access JWT を拒否する。
- [x] Tailscale profile は invalid / expired / wrong-resource OAuth token を拒否する。
- [x] local OAuth は PKCE downgrade / code replay / redirect mismatch を拒否する。
- [x] OpenAI profile は local origin の public bind を必要としない。
- [x] provider-neutral MCP dispatch に Cloudflare / Tailscale / OpenAI 固有 process control が漏れない。
- [x] `session_start` / `session_stop` / named roots security semantics が3 profileで同一である。
- [x] `without_sandbox` が remote tool list に現れない。
- [x] `doctor` が選択 profile に不要な dependency / secret を failure にしない。
- [ ] 3 profile の live acceptance evidence を残す。
- [x] README / `docs/public-http*.md` / development docs を connection profile architecture に更新する。

## 実装 evidence (2026-08-23)

### Common / regression

- `cargo fmt -- --check`: pass
- `cargo clippy --all-targets --all-features -- -D warnings`: pass
- `cargo test --all-targets --all-features`: 223 passed / 0 failed / 1 intentional process-boundary E2E ignored
- `cargo check --all-targets`: pass
- remote tool list regression で `without_sandbox` 非公開を確認
- Cloudflare / Tailscale / OpenAI は同じ `SessionSupervisor` / named-root / sandbox / local approval path を利用し、provider 固有 process control は connection boundary の外へ漏らさない

### Cloudflare

`./target/debug/temote-mcp doctor --profile cloudflare`:

- `cloudflared 2026.6.1`: pass
- tunnel token file mode `0600`: pass
- Access team domain / audience / email allowlist: pass
- summary: `0 failure(s), 0 warning(s)`

Cloudflare Access JWT rejection / unauthenticated `/mcp` は regression tests で維持している。実 Cloudflare Managed OAuth client の browser/external live acceptance はこの作業では未実施のため Phase 5 の該当項目は open のままにする。

### Tailscale + local OAuth live acceptance

実 host には既存 Funnel `:443 -> http://localhost:7135` がある状態で検証した。Temote はこれを変更せず、利用可能な `8443` を選択した。

`temote-mcp doctor --profile tailscale`:

- connected `*.ts.net` node: pass
- existing `{443}` ownership detection: pass
- selected safe port `8443`: pass
- local OAuth state: pass
- summary: `0 failure(s), 0 warning(s)`

外部 HTTPS `:8443` を通した acceptance:

- protected-resource metadata: pass
- authorization-server metadata: pass
- DCR registration: pass
- local terminal owner approval: pass
- Authorization Code + PKCE S256: pass
- access-token exchange: pass
- authenticated `tools/list`: pass
- `without_sandbox` absent: pass
- managed `session_start`: pass (`yolo=false`)
- managed `session_stop`: pass
- shutdown 後 `:8443` foreground Funnel のみ消滅: pass
- 既存 `:443 -> http://localhost:7135` が before/during/after で不変: pass

### OpenAI Secure MCP Tunnel

OpenAI official docs / `openai/tunnel-client` の current surface を確認し、Temote は public ingress abstraction へ押し込めず private connection profile として実装した。

- local MCP origin は loopback-only: regression pass
- `TEMOTE_MCP_PUBLIC_URL` 不要: pass
- Cloudflare / Tailscale dependency 不要: pass
- Temote-owned `tunnel-client run --control-plane.tunnel-id ... --mcp.server-url http://127.0.0.1:<port>/mcp` lifecycle: implemented
- interactive setup/up は sudo-style hidden TTY prompt を使用でき、secret を argv / config / log に出さない; non-interactive runtime は `CONTROL_PLANE_API_KEY` / official `OPENAI_API_KEY` fallback を維持
- prompted Runtime API key は Temote-owned `tunnel-client` child environment にだけ注入し、`OPENAI_ADMIN_KEY` は runtime child から明示的に除去、prompt buffer は spawn 後 zeroize
- tunnel ID grammar / no-public-bind property tests: pass
- official `openai/tunnel-client` を `/tmp` で source buildし version `0.0.13-dev+5ce4fed0a730da034c54c1de17f2610f8b2727f1` を Temote doctor が認識: pass
- `temote-mcp openai setup --workspace-id ...` が official `POST /v1/tunnels` contract を使う bootstrap command として追加済み
- setup API contract test: admin bearer header / workspace scope / returned tunnel ID parse: pass
- hidden prompt / runtime child environment regression: prompted key injection + admin/fallback key stripping tests: pass
- saved `~/.config/temote-mcp/openai.env` は `CONTROL_PLANE_TUNNEL_ID` のみ・mode `0600`; API key を含む config は reject: pass
- existing saved tunnel ID は `--force` なしで replacement を拒否: implemented
- current host には `OPENAI_ADMIN_KEY` / `CONTROL_PLANE_API_KEY` がないため production API create / supported OpenAI product live acceptance は未実施

OpenAI control-plane / supported OpenAI product との live tunnel acceptance は credential / workspace entitlement がこの host に無いため未実施。Cloudflare/Tailscale へ暗黙 fallback せず、Phase 5 の OpenAI live 項目を open のままにする。

## テスト方針

- Cloudflare provider regression tests
- provider-neutral connection/auth dispatch tests
- local OAuth state-machine tests
- authorization code single-use PBT
- redirect URI exact-match PBT
- resource / audience binding PBT
- token expiry / replay PBT
- profile-specific environment validation tests
- lifecycle ownership tests
- `doctor` provider matrix tests
- OpenAI profile no-public-bind regression test
- Cloudflare live acceptance
- Tailscale Funnel live acceptance
- OpenAI Secure MCP Tunnel live acceptance

既存 `noprop` PBT suite を security boundary の不変条件へ積極的に適用する。

## 設計上の判断

### Cloudflare と Tailscale は public provider として直交化する

`cloudflare` / `tailscale` を巨大な条件分岐として MCP core に入れない。public ingress と auth を別責務にする。

### OpenAI Secure MCP Tunnel は無理に public ingress model へ入れない

Secure MCP Tunnel の価値は local/private MCP server を public Internet に露出しないことにある。そのため `PublicEndpoint` や public OAuth server を必須とする abstraction に合わせない。

### support matrix は絞る

正式サポートはまず以下だけとする。

```text
cloudflare = Cloudflare Tunnel + Cloudflare Access
tailscale  = Tailscale Funnel + Temote local OAuth
openai     = OpenAI Secure MCP Tunnel
```

### Cloudflare を legacy 扱いしない

managed identity / policy、custom domain、Workers / Durable Objects integration の利点があるため第一級 backend として維持する。

### Tailscale を認証そのものにしない

public MCP の Tailscale profile では Funnel は ingress、local OAuth は auth と分離する。

### OpenAI profile は OpenAI-only 接続に最適化する

OpenAI Secure MCP Tunnel は supported OpenAI products との private connection に使う。general-purpose MCP endpoint が必要なら Cloudflare / Tailscale を使う。

## リスク

- self OAuth 実装は security-sensitive であり、単なる tunnel backend 追加より大きな変更になる。
- MCP authorization specification と主要 client compatibility を実装時点で再確認する必要がある。
- Tailscale CLI / Funnel lifecycle ownership を誤ると Temote 外の設定へ干渉する可能性がある。
- Cloudflare abstraction 時に existing Access defense-in-depth を弱めない必要がある。
- OpenAI Secure MCP Tunnel は OpenAI product / workspace entitlement / service availability / evolving beta semantics に依存する可能性がある。
- Secure MCP Tunnel の undocumented behavior を推測して contract 化しない。
- provider abstraction を一般化しすぎず、実際の3 profile から必要な contract のみ抽出する。

## 公式情報

OpenAI Help Center（2026-08-23確認）では、ChatGPT は local MCP server へ直接接続せず、private network / on-premises / developer machine 上の MCP server を public Internet に露出せず supported OpenAI products に接続する用途として Secure MCP Tunnel を案内している。

- https://help.openai.com/en/articles/12584461-developer-mode-and-full-mcp-connectors-in-chatgpt
- https://github.com/openai/tunnel-client

仕様・availability は変化し得るため、実装開始時に公式情報を再確認する。

## 変更履歴

`CHANGES.md` impact: n/a（repository に CHANGES.md は存在しないため README / docs / issue evidence を更新）
