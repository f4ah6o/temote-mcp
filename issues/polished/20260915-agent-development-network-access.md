# Agent mode で通常の開発ネットワークを利用可能にする

Status: polished
Model: deepseek-v4.1-flash
Created: 2026-09-15
Updated: 2026-09-16
Branch: main
Priority: P1 developer experience

## 概要

`PermissionMode::Agent` を、sandbox / path containment を維持したまま通常の開発作業を approval-free で進められる実用的な開発モードにする。

現在の agent mode は filesystem / process の sandbox 境界を維持しつつ approval を抑えられる一方、通常の `execute` / `start_command` は network-disabled の sandbox profile を使う。そのため、開発中に `curl`、`ffprobe`、RTSP/HTTP 疎通確認、package registry、dev server、localhost/LAN service などへアクセスする ordinary command が必要になると、人間による代行や別 operation class が必要になる。

この issue では **agent mode の ordinary command に通常の開発ネットワークを許可する**。sandbox 外実行や public yolo 昇格は導入しない。

## 現状

Permission mode と network policy は現在別の実装軸になっている。

- `ask`: 通常 sandbox + approval + ordinary command network restricted
- `agent`: 通常 sandbox + approval-free を基本とするが ordinary command network restricted
- `yolo`: local-only unrestricted mode。通常の Temote sandbox/path restriction を外し、host user の filesystem/process/network 権限を使う

既存 sandbox 実装には network-enabled profile がすでに存在する。

- `local_agent_run` はモデルサービス等への通信のため network-enabled profile を使う
- 一部 `dev_tool_run` operation は dependency/network operation のため network-enabled profile を使う
- macOS sandbox は network enabled 時に outbound network を許可できる
- Linux sandbox は restricted 時だけ network namespace / seccomp で network を遮断し、network-enabled profile では host network を利用できる

一方、ordinary `execute` / `start_command` は agent mode でも network restricted のままである。

## 問題

agent mode の目的は「sandbox 境界を維持しながら、開発作業を原則 approval-free で継続できること」だが、ordinary command の network restriction により日常的な開発操作が不必要に停止する。

代表例:

- `curl https://...` による API / artifact / documentation endpoint の疎通
- `ffprobe rtsp://192.168.x.x/...` による LAN camera / media stream の確認
- localhost 上の dev server / test server への client access
- package registry / dependency service への ordinary CLI access
- LAN 上の development device / service との HTTP, RTSP, WebSocket, TCP 通信
- local development server の bind/listen とその E2E test

これらを yolo に切り替える必要はない。filesystem/path sandbox と public MCP の安全境界はそのまま維持したい。

## 目標

`PermissionMode::Agent` の ordinary `execute` / `start_command` を、sandbox 内のまま通常の開発ネットワークを利用できるようにする。

期待する semantics:

```text
ask:
  sandbox = yes
  ordinary command network = restricted
  approval = strict

agent:
  sandbox = yes
  ordinary command network = development-enabled
  approval = normally unnecessary

yolo:
  sandbox = no
  host network/process/filesystem = unrestricted
  local only
```

## 方針

最初の implementation slice では session-level の任意 network policy selector は追加しない。

`PermissionMode::Agent` の ordinary command sandbox construction を既存の network-enabled sandbox primitive に接続する。permission mode と network policy の内部実装を完全に同一概念へ統合する必要はないが、外部 semantics として agent mode なら通常の開発通信が止まらないことを保証する。

### 維持する境界

- permitted roots / path containment を維持する
- protected metadata / repository safety restriction を維持する
- public MCP から `without_sandbox` を利用できない状態を維持する
- public MCP から `yolo` へ昇格できない状態を維持する
- local-only yolo semantics は変更しない
- host firewall / OS permission / service ACL を bypass しない

### Network semantics

最初の slice は LAN-only filtering を実装しない。

agent mode の network-enabled sandbox は、host から到達可能な localhost / LAN / Internet outbound 通信を通常の開発用途として許可する。CIDR 単位の宛先制限や Internet deny + LAN allow のような destination-aware policy は別 issue とする。

bind/listen については platform sandbox の実挙動を acceptance test で確認し、localhost/LAN development server に必要な socket operation が不足している場合のみ sandbox policy を最小限拡張する。

## 対象範囲

- `execute` の agent mode network behavior
- `start_command` の agent mode network behavior
- macOS sandbox policy の network-enabled ordinary command path
- Linux sandbox policy の network-enabled ordinary command path
- permission mode ごとの regression tests
- localhost / LAN / Internet development network acceptance
- docs の permission-mode semantics 更新

## 対象外

- public MCP からの yolo 作成・昇格
- public `without_sandbox`
- unrestricted host filesystem/process access の agent mode への導入
- CIDR / hostname / port 単位の allowlist
- LAN-only firewall の実装
- credential / secret の自動注入
- host firewall や OS privacy permission の回避
- yolo semantics の変更

## Implementation slices

### Slice A — ordinary command network enablement

- agent mode の `execute` / `start_command` が network-enabled sandbox profile を使うようにする
- ask mode は既存の network-restricted behavior を維持する
- yolo は既存 local-only unrestricted behavior を維持する
- macOS / Linux の sandbox construction に mode-specific regression test を追加する

**Done when:** agent mode の ordinary command が sandbox 内から network を利用でき、ask mode の restriction と yolo の unrestricted semantics が回帰していない。

### Slice B — development network live acceptance

実際の process/socket behavior を platform ごとに検証する。

- outbound HTTPS
- localhost client/server
- LAN HTTP/TCP access
- RTSP access where a reachable fixture/device is available
- bind/listen required by dev/test servers

実 LAN device に依存する確認は repository-local deterministic test と分離し、live acceptance evidence として記録する。

**Done when:** ordinary development workflow で人間による network command 代行を必要としないことが実測できる。

### Slice C — docs / compatibility closure

- README / usage / managed-session docs の ask / agent / yolo semantics を更新する
- agent mode が sandboxed であり、network-enabled が yolo と同義ではないことを明記する
- public MCP の yolo / without_sandbox 制約が変更されていないことを明記する

**Done when:** operator と client が agent mode の安全境界と network capability を誤解しない。

## 受け入れ条件

- [ ] agent mode の `execute` から HTTPS endpoint へ接続できる
- [ ] agent mode の `start_command` から network を使う long-running development process を起動できる
- [ ] agent mode から localhost service へ接続できる
- [ ] agent mode で localhost development server を bind/listen し client connection を受けられる
- [ ] host から到達可能な LAN HTTP/TCP service へ agent mode から接続できる
- [ ] reachable な RTSP fixture/device がある場合、agent mode の `ffprobe` 等から直接疎通できる
- [ ] ask mode ordinary command の network restriction は維持される
- [ ] permitted root 外への write は agent mode でも拒否される
- [ ] protected repository/metadata restriction は agent mode でも維持される
- [ ] public `without_sandbox` は引き続き拒否される
- [ ] public MCP から yolo へ昇格できない
- [ ] local yolo semantics は変更されない
- [ ] macOS / Linux の deterministic regression tests が追加される
- [ ] permission-mode docs が実装 semantics と一致する

## テスト計画

Repository-local deterministic tests:

- ask + ordinary command => network restricted profile
- agent + ordinary command => network-enabled sandbox profile
- yolo => existing local-only unrestricted path
- permitted-root write => pass
- outside-root write => fail
- protected metadata write => existing restriction preserved
- public `without_sandbox` => fail
- public yolo promotion => fail
- sandbox profile generation / Linux policy selection regression tests
- `git diff --check`

Live acceptance:

```sh
curl https://example.com
```

```sh
python -m http.server 8765 --bind 127.0.0.1
curl http://127.0.0.1:8765/
```

LAN fixture/device がある場合:

```sh
curl http://192.168.x.x/
ffprobe rtsp://192.168.x.x:8554/stream
```

Live tests は対象 LAN device が存在しない環境で repository-local completion を不可能にしない。fixture-dependent evidence と deterministic policy tests を分離する。

## リスク

agent mode の network-enabled 化により、sandbox 内 process が host から到達可能な network endpoint へ接続できるようになる。これは開発モードとして意図した capability expansion だが、filesystem/path sandbox の解除とは分離して扱う必要がある。

network enabled を yolo と同義にせず、public MCP の sandbox escape / yolo promotion を引き続き禁止する。将来 destination-aware network restriction が必要になった場合は、現在の boolean/profile-level network enablement を拡張する別設計として扱う。

## CHANGES.md impact

yes

項目案:

- Agent permission mode の ordinary commands で sandbox を維持した development network access を有効化。

## Recommended first implementation

1. `PermissionMode::Agent` の ordinary `execute` / `start_command` sandbox construction を既存 network-enabled primitive に接続する。
2. ask/agent/yolo の mode matrix test を先に追加する。
3. macOS で outbound + bind/listen、Linux で host network access + sandbox containment を検証する。
4. public yolo / `without_sandbox` regression を必ず同時確認する。
5. LAN-only filtering はこの issue に持ち込まない。

## Triage note

- 2026-09-16: Slice A〜C、受け入れ条件、deterministic/live テストの分離が揃っており実装に着手できるため `ready` と判定し、`issues/open/` から `issues/polished/` へ移動した。この変更は `AGENTS.md` の safety invariant「Normal `execute` / `start_command` run in the sandbox with network disabled」を agent mode について更新するため、実装と同じ変更で AGENTS.md と permission-mode docs を明示的に更新し、public yolo / `without_sandbox` の回帰を同時に確認すること。
