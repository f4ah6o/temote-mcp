# 1Password `op` read path を batch / coalescing / optional local fast-read で高速化する

Status: open
Model: gpt-5.6-sol
Created: 2026-09-01
Updated: 2026-09-01
Branch: main

## 概要

Temote MCP の 1Password read path について、公式 `op` CLI の security model を維持したまま batch 化と request coalescing で固定費を償却し、将来的に明示 opt-in の read-only local fast path を追加する。

直接 SQLite write は行わない。write / mutation は公式 1Password 経路だけを利用する。

## 実機調査結果

2026-09-01、macOS 26.5.2 / arm64 / 1Password CLI 2.34.1 で測定した。

| 操作 | 実測 |
| --- | ---: |
| `op --version` | 約 10 ms |
| `op item list --vault Personal --format json` cache ON | median 1.40 s |
| 同 cache OFF | median 4.04 s |
| `op item get` cache ON | median 1.76 s |
| 同 cache OFF | median 2.37 s |
| 10 item を個別 `op item get` | 19.91 s |
| 10 item を `op item get -` で batch | 6.74 s |
| local `1password.sqlite` open | 約 33.5 ms |
| local item query | median 約 0.002 ms |

10 item の batch 化だけで約 2.96x 高速化した。

`OP_DEBUG=true` では、キャッシュ済み item でも次の経路を通ることを確認した。

```text
VaultItems: cache hit
-> Item: VaultItems cache hit ... validating staleness using item version
-> Item: cache hit
```

`op item get` 実行中には Unix socket に加えて外向き TLS `:443` 接続も観測した。`op daemon` は cache/session 管理を行うが、新規 isolated daemon 経由でも `item get` は median 約 2.57 s であり、daemon 常駐化だけでは主ボトルネックを解消しない。

公式 CLI バイナリには `OP_CACHE`, `OP_SOCK`, `op-daemon.sock`, `VaultItems cache hit/miss`, `Item cache hit/miss` が存在する。

## direct DB feasibility

`jeremyschlatter/opcli` の方式も実機で検証した。

- 1Password desktop の `1password.sqlite` を read-only で読む
- 2SKD / AES-256-GCM / RSA-OAEP で local decrypt
- network / Desktop IPC を使わない
- 現 host の DB schema version 61 と account metadata の認識までは確認
- upstream HEAD `569cc37` は通常 build 成功
- 同 HEAD の `go test ./...` は generated helper 不足で失敗
- 初回 signin で独自 Keychain に account password を保持するため、そのまま依存すると公式 Desktop integration より security boundary が悪化する

したがって upstream `opcli` をそのまま production dependency にせず、local fast-read は別 phase で security design を確定してから必要最小限を実装する。

## 目標

1. 複数 item read を一回の公式 `op item get -` にまとめられる Temote bridge を追加する。
2. 同一 session 内の近接・重複 read を coalesce し、同一 item を重複取得しない。
3. secrets を activity / approval summary / session metadata / ordinary diagnostic logs に出さない。
4. official CLI batch path は macOS/Linux の既存 1Password CLI authentication を利用する。
5. optional local fast-read は read-only / explicit opt-in とし、write path と完全分離する。
6. direct SQLite write は禁止する。

## 非目標

- 1Password Desktop の署名検証を回避して private IPC に接続すること。
- 1Password database を直接変更すること。
- 独自 sync / conflict resolution を実装すること。
- `OP_SERVICE_ACCOUNT_TOKEN` を agent-visible output や metadata に出すこと。
- upstream `opcli` を無検証で vendoring / dependency 化すること。

## Phase 1: official CLI batch read

- [x] `onepassword_item_get` MCP tool を追加する。
- [x] `items` は複数指定可能にし、一回の `op item get - --format=json` に集約する。
- [x] optional `vault` / `account` をサポートする。
- [x] item 数・各 query 長・総入力サイズを bound する。
- [x] `op` stdout/stderr/output size を既存 bounded process runner で制限する。
- [x] raw secret/value を activity や approval summary に含めない。
- [x] write / edit / create / delete command をこの bridge から実行できないことをテストする。
- [x] gateway routed-tool contract を更新する。
- [x] docs / Agent Skill を更新する。

## Phase 2: request coalescing

- [x] session-scoped bridge を導入し、同一 `(account, vault)` の近接 read を micro-batch する。
- [x] 同一 explicit batch と cross-call micro-batch の item query を resolved item ID で deduplicate する。
- [x] caller ごとに結果を正しく fan-out する。
- [x] one caller cancellation が shared read を壊さないようにする。
- [x] coalescing window と max batch size を bounded にする。
- [x] secret plaintext を durable cache に保存しない。

## Phase 3: optional local fast-read

- [ ] macOS 1Password local DB schema compatibility boundary を定義する。
- [ ] read-only open を fail-closed で保証する。
- [ ] schema/version mismatch は公式 CLI path へ fallback する。
- [ ] credential/key handling を threat model 化する。
- [ ] 公式 Desktop integration より弱い password persistence を default にしない。
- [ ] explicit opt-in のみ許可する。
- [ ] local DB write API を実装しない regression test を置く。

## Security invariants

- `onepassword_item_get` は read operation のみ。
- item query は secret ではないものとして扱うが、取得結果は secret-bearing として扱う。
- activity は item count / success/failure 程度だけを表示し、item title、field value、password、token を表示しない。
- normal session でも host 1Password CLI の既存 authorization / Desktop authentication semantics を弱めない。
- child process environment から Temote が保持する他 integration credentials を不要に継承させない。
- direct DB fast path は read-only FD / SQLite mode=ro 相当を必須にする。
- mutation は official 1Password API/CLI/SDK path に限定する。

## Acceptance criteria

- [x] 10 item read が個別 `op item get` より明確に高速である。
- [x] live host で batch result が個別 result と意味的に一致する。
- [x] item query の重複が一 batch 内で一度だけ upstream に送られる。
- [x] malformed / oversized inputs を `op` 起動前に拒否する。
- [x] secret-bearing output が approval/activity/debug summary に混入しない。
- [x] existing `onepassword_mcp_*` / service-account behavior を壊さない。
- [x] `cargo fmt --all -- --check`
- [ ] `cargo test`
- [x] `cargo clippy --all-targets -- -D warnings`
- [x] `cargo check --no-default-features --all-targets`
- [x] `(cd gateway && npm test)`
- [x] `git diff --check`

## Phase 1 implementation result (2026-09-01)

- `onepassword_item_get` を Rust MCP surface / gateway contract に追加。
- exact item ID を優先し、次に exact title を解決する。title が曖昧なら fail-closed。
- resolved item ID を deduplicate し、overview JSON array を stdin に渡して一回の `op item get -` へ集約する。
- live host で 10 item: **7.66 s**。事前計測の10回個別取得 **19.91 s** に対して約 **2.60x**。
- live host で3 item batchと3回個別取得の JSON semantic equality を確認。
- `[A, A, B]` request が2 item resultになることをlive確認。
- new unit tests 6/6 pass。gateway tests 46/46 pass。Clippy / no-default-features check pass。
- full Rust suite は `cargo test -- --test-threads=1` で332/332 pass。通常の並列 `cargo test` では既存 `supervisor::tests::on_failure_policy_restarts_crash_and_graceful_stop_does_not_restart` が4秒timeoutで断続的にfailし、同test単独実行はpass。今回のfeature差分には同testの変更を含めない。


## Phase 2 implementation result (2026-09-01)

- MCP process 内に session + `(account, vault)` keyed bridge を追加。scope bridge は最大64、各scope pending queueは128、micro-batch windowは15 msにboundした。
- 一つのmicro-batchでは最大100 queryまで受け、`op item list` を1回共有してcaller別にresolveする。曖昧title/missing queryはそのcallerだけをfailさせる。
- 成功callerのresolved IDをunion/deduplicateして `op item get -` を1回実行し、返却itemをIDでcallerごとの元順序へfan-outする。
- oneshot receiverがdropされたcallerへの送信失敗は無視し、shared batchと他callerの応答を継続する。
- fetched secret plaintextはbatch処理中のmemoryにだけ存在し、durable cache/session metadata/activity/approval summaryへ保存しない。
- unit tests: 11/11 pass。micro-batch collection、overflow defer、session scope分離、ambiguity isolation、ID fan-out、caller cancellation、batch-size PBTを含む。
- fake `op` を使うloopback HTTP process-boundary E2Eで、同時2 callerが `item list` 1回 + `item get` 1回へcoalesceされ、各callerへ正しいIDが返ることを確認。
- current `gateway-agent` poll loop はhost RPCを直列dispatchするため、gateway-routed callsでは同時callが同一processへ到達せずcross-call coalescingの効果は出ない。global gateway dispatch concurrencyはtool ordering/approval semanticsへ影響するため、このfeatureでは変更しない。explicit `items` batchはtransportに依存せず有効。

## 設計判断

最初に official CLI batch path を採用する。実機で約 3x の改善が確認でき、1Password の既存認証・暗号化・sync semantics をそのまま利用できるため、リスクに対する効果が最も大きい。

local SQLite direct-read は性能上の上限を引き上げられるが、credential handling と schema compatibility の security/maintenance cost が高い。そのため Phase 1/2 の測定後に opt-in fast path として判断する。
