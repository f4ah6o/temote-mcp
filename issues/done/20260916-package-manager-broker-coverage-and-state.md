# `uv` / `npm` / `pnpm` / Go の dependency operation に structured developer capability がない

Status: doing / first slice implemented; live agent acceptance pending
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 developer workflow friction
Type: developer broker / package managers / sandbox

## Observed

current source の `dev_tool_run` は `cargo | vp` のみを受理する。

```text
DevTool::Cargo
DevTool::Vp
```

Cargo/Vite+ は `dev_offline` と `dependency_network` を明示分類し、後者だけ scoped network capability と tool-state roots を使える。一方、日常開発で使う `uv`, direct `npm`, direct `pnpm`, Go module operation には同等の structured route がない。

ordinary `execute` は network-disabled なので、direct package-manager network operation は失敗する。今回 `npm ping --registry=https://registry.npmjs.org` を normal `agent` session で実行したところ background job になった後、terminal `failed` になった。

```text
npm notice PING https://registry.npmjs.org/
npm error code EAI_AGAIN
npm error syscall getaddrinfo
npm error errno EAI_AGAIN
npm error request to https://registry.npmjs.org/-/ping failed, reason: getaddrinfo EAI_AGAIN registry.npmjs.org
npm error Log files were not written due to an error writing to the directory: /home/hirohito-fujita/.npm/_logs
```

つまり expected network denial に加え、npm の diagnostic log path も read-only で secondary warning が発生する。

state/cache の現状 probe:

```text
uv cache dir
/home/hirohito-fujita/.cache/uv

npm config get cache
/home/hirohito-fujita/.npm

pnpm store path
/home/hirohito-fujita/src/local-mcp/node_modules/.pnpm-store/v11

go env GOCACHE GOMODCACHE GOPATH
/home/hirohito-fujita/.cache/go-build
/home/hirohito-fujita/go/pkg/mod
/home/hirohito-fujita/go
```

`issues/done/20260916-agent-mode-language-cache-writable-defaults.md` は ordinary sandbox の private `XDG_CACHE_HOME` / `GOCACHE` first slice と Go module-cache overlay を追跡している。本 issue は **network-capable dependency operation の structured capability coverage** と package-manager-specific mutable state を扱い、単純な cache env 修正とは分離する。

## Problem

利用者視点では language/package manager ごとに「Cargo/Vite+ は broker を使える、npm/uv/pnpm/go は raw execute なので network が使えない」という非対称性を覚える必要がある。

依存取得のために session 全体を yolo にするのは過剰であり、ordinary sandbox に network を常時開放するのも不適切。既存 `dev_tool_run` の capability model を supported toolchain へ拡張する方が一貫する。

## Proposed direction

`dev_tool_run` の tool set を一括 unrestricted command にせず、tool ごとに operation classifier を追加する。

候補:

- `uv`
  - offline: `run` の扱いは arbitrary project code execution を含むため要精査
  - dependency-network: `sync`, `lock`, `add`, `remove`, `pip install` 等を個別分類
  - cache/state: uv cache と Python environment path の write boundary を明示
- `npm`
  - dependency-network: `install`, `ci`, `update`, `view`, `ping` 等を read/query と mutation に分ける
  - lifecycle scripts による arbitrary code execution を別 class とする
  - `~/.npm/_logs` / cache を private or scoped state として扱う
- `pnpm`
  - shared content-addressable store と project `node_modules` を分離
  - install/update/fetch と script execution を分離
- `go`
  - module download/update (`mod download`, `get`, `list -m` 等) と build/test を分離
  - read-only module contents + mutable metadata/cache の overlay 問題を既存 cache issue と連携

## Security constraints

- tool name allow-list だけで安全扱いしない。npm/pnpm/uv/Go の project/lifecycle/build hooks は code execution capability を持つ。
- arbitrary raw argv / executable / registry URL / credential value をそのまま host capability にしない。
- network enablement は exact operation class に紐付ける。
- registry credentials、npmrc/pypi auth、Go private module credentials を output/log/session metadata に出さない。
- host HOME 全体を writable にしない。
- package store/cache の既存 contents を offline reuse する必要と、mutable metadata を private upper へ分ける必要を区別する。

## Implemented first slice (2026-09-16)

`dev_tool_run` の tool enum を `cargo | vp | uv | npm | pnpm | go` へ拡張し、package-manager側は最初から raw argv を許可せず bounded operation table とした。

- `uv lock`
  - `dependency_network`
  - caller-supplied args は拒否
  - `--no-build --no-python-downloads` を broker が強制し、PEP 517 build backend / managed Python download を first slice から外す
- `npm install|ci|update|ping|outdated`
  - `dependency_network`
  - caller-supplied args は拒否
  - install/ci/update は `--ignore-scripts` と `npm_config_ignore_scripts=true` を broker が強制
- `pnpm install|fetch|update|outdated`
  - `dependency_network`
  - caller-supplied args は拒否
  - install/update は `--ignore-scripts`、install/update/fetch は `--ignore-pnpmfile` を broker が強制し、lifecycle scripts と `.pnpmfile.cjs` hook の両方を first slice から外す
- lifecycle script second phase
  - `npm rebuild` と `pnpm rebuild_pending` (`pnpm rebuild --pending`) を `dev_offline` に分類
  - dependency network operation と lifecycle code execution を同じ phase に置かず、install/fetch 時は scripts off、rebuild 時は network off とする
  - raw `pnpm rebuild` は引き続き rejected とし、`--pending` 固定の public operation のみ許可する
- `go mod_download`
  - public operation 名は shell fragment を受けない `mod_download`
  - exact argv は `go mod download`

package-manager state は `$TEMOTE_STATE/developer-tools/<tool>` 相当の Temote-owned root を approval 後に作成し、実体 directory / symlink rejection / Unix 0700 を検証する。npm cache、pnpm store/state、uv cache、Go `GOCACHE/GOMODCACHE` はこの root へ固定し、package-manager用には既存 HOME cache/state を追加 writable にしない。Cargo/Vite+ の従来 state-root contract は維持する。

Repository-local verification:

- `cargo test --bin temote-mcp dev_tool::tests::`: 16/16 PASS
- classifier PBT (`generated_operations_classify_deterministically_and_fail_closed`): existing 1,024-case noprop loopを6 toolへ拡張して PASS
- private state root test: real directory + mode 0700 + symlink rejection PASS
- package-manager host HOME state writable-root reuse denial test: PASS
- routed gateway contract は deterministic generator で更新済み
- `npm test --prefix gateway`: 2/2 PASS
- `cargo clippy --all-targets -- -D warnings`: PASS
- `just sandboxed-check`: exit 0。library 110/110、activity 5/5 + 3/3、upgrade 40/40 + 6/6、gateway 2/2、fmt/clippy/no-default/diff-check PASS。nested Linux sandbox / full local Unix-socket / process-boundary E2E は明示 `NOT RUN (host/CI gate)`。

live network acceptance は current ChatGPT-connected Temote runtime が source更新前の tool schema のため未確認。2026-09-16 の exact action discovery `dev_tool_run` でも `dev_tool_run` 本体は返らず、旧 connected surface のままである。ordinary outer sandbox から raw package-manager networkを開けて確認することはしない。

## Acceptance criteria

- [ ] `uv`, `npm`, `pnpm`, `go` の supported dependency operations が normal `agent` session から明示的 structured network capability で実行できる。
- [x] unsupported/arbitrary-code-sensitive operation は fixed classification で拒否される。
- [x] direct ordinary `execute` の network-disabled contract は維持される。
- [x] npm の failure diagnostic が `~/.npm/_logs` read-only という secondary warningを避ける Temote-owned cache path を supported broker に実装する。
- [x] tool-specific state roots は最小化され、HOME 全体 writable にはならない。
- [x] package manager ごとの table-driven tests と generated/PBT coverage を追加する。
- [x] Cargo/Vite+ の既存 classifier / network / sandbox tests を回帰させない。

## 2026-09-22 consolidation: done

Repository-local implementation verified on `main`; remaining live/canary evidence is tracked in `issues/open/20260908-live-acceptance-matrix.md` under "Repository-completion live residuals".
