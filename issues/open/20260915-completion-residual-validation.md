# 2026-09-15 completion work の残存 review・評価・実機確認を完了する

Status: open
Model: gpt-5.6-sol
Created: 2026-09-15
Updated: 2026-09-15
Branch: codex/20260915-complete-open-work

## 概要

2026-09-15 の completion work は実装候補と大部分の repository-local gate まで完了したが、独立 review、比較評価、実 provider / 対応 OS の確認が残っている。
本イシューは利用枠による停止後の再開点を一か所に保持し、未確認の項目を完了扱いしないための residual tracker である。

## 今回の再開結果（2026-09-15）

状態は **open / blocked**。再開時のintegration HEADは `b02bafe0f6a4e0c9ee8c3d7788fe92beb08fede3`、activityは `8a6efff690963a0334757b161ad9729f6cd52db0`、evaluationは `cf9d1ffe69638a27af3f39c6b19a1b5ac704ae50` だった。下記の初回停止時点のSHAは履歴として保持する。既存worktreeと未コミット変更は破棄せず、credential・remote・installed runtimeは変更していない。

1. **session**: 指定の `temo` は存在せず、既存 `temote` は `crashed` / `session was active when its owning supervisor stopped`。設定済みnamed root `src` から `temo` と各completion worktree専用sessionを作成した。いずれもLinux host `ms-01-alpha` の `active` / `permission_mode=agent` / `yolo=false` を確認した。
2. **独立review**: S15/S16のread-only Codex reviewerは起動失敗。JSON-RPC `-32000`、exit 1、`bwrap: execvp /home/hirohito-fujita/.cargo/bin/codex: No such file or directory`。job listは空だった。別途 `codex_status` も `CODEX_APP_SERVER_INCOMPATIBLE: expected 0.153.4, got temote-mcp/0.153.4 (Ubuntu 24.4.0; x86_64) unknown (temote-mcp; 2026.9.7)` で失敗した。S15/S16は未承認であり、activity codeはintegrationへ取り込んでいない。
3. **親の部分静的review**: detached upgradeは旧session tuple（ID・開始時刻・PID）の比較後に現在runtimeのnonceを取得する。同一tuple・異なるnonceでの再作成をupgrade経路でも拒否できるか、独立reviewと決定的テストが必要。通常のbound updateは元のnonceを保持する。liveで誤帰属を再現したとは扱わない。新しくawaitするactivity bindとnonblocking要件の整合も未判定。詳細はactivity branchの `docs/evaluations/completion-residual-validation-20260915.md` に記録した。
4. **candidate gate**: private HOME/CODEX_HOME/XDG/TMPDIR/socket namespace、offline Cargo、clone-local targetでS16 HEADを検査した。fmt、strict all-target Clippy、no-default check、diff checkはPASS。lib testsは99 PASS / 8 FAILで複数の失敗が `/var/tmp` read-only（OS error 30）。producerは3 PASS / 4 FAIL、ingressは3 PASS / 6 FAIL、lifecycleは0 PASS / 3 FAIL、gatewayはexit 1（返却されたtop-level集計は1 PASS / 1 FAIL）。後者の個別原因は未確定。full unfiltered test、最終integration gate、明示process-boundary E2Eは未実行。過去のcandidate 954 PASS / gateway 70 PASSを今回の成功と読み替えない。
5. **実際のjob状態**: `a87f2986-9884-4cd6-a2e6-9eb866397ebd` はpollでexit 1、後続job listで `failed`。activity sessionは `active`。追加source確認とfailure-log確認の2 tool callは `リクエストの安全性を確認できなかったため、このツールの呼び出しは OpenAI によってブロックされました。` と返された。当該操作は別経路で再試行せず、session全体の停止とは扱っていない。
6. **評価と実機**: T01-A / T06-B review、T05-C、T07-C、T08-C/A、T09/T10をresultsに明示的なblockedとして記録した。過去の試行分母、unknown、T05-Cのpartial commitと最後の観測stateは維持した。既存4 issueには実provider認証、macOS実機、physical multi-host、sandboxを維持したLinux実行環境などの具体的な不足条件を追記し、openを維持した。
7. **resumeの説明**: `Applied` はreconciliationの受付結果であってtask完了ではない。既存の `Applied + Interrupted`、replacement process PIDの直接記録なし、resume後のcompleted例なしを英日usage、Agent Skill、resultsで一致させた。これはdocumentation-onlyの明確化で、新しいlive成功ではない。

最終commit / push結果は、この追記を含むdocumentation差分の検査後に記録する。独立review、必要な修正・再review、最終integration gateは未完了のため、本issue全体はcloseしない。

## 背景

- integration branch `codex/20260915-complete-open-work` は `62d836cdc428c53a9370e3964b88d3f111ae5f64`。
- activity branch `codex/20260915-completion-activity` は S15 実装 `5be77826b500b7f0a9bdc18e1731e320dedd0ffb` と S16 docs / `CHANGES.md` `8a6efff690963a0334757b161ad9729f6cd52db0` を保持する。
- evaluation branch `codex/20260915-completion-evaluation` は `b0a3f7bd0ff01185623723febad8f077843ca304`。詳細は [manifest](../../docs/evaluations/completion-evaluation-manifest-20260915.md) と [results](../../docs/evaluations/completion-evaluation-results-20260915.md) にある。
- origin への push は `Permission to f4ah6o/temote-mcp.git denied to fujita-obr` の HTTP 403 で失敗した。credential / remote は変更せず、権限のある環境から上記3 branchを再 pushする。

## 問題

### activity S15 / S16

S15 `5be7782` は lifecycle、upgrade activity、private session-instance nonce fence を実装済みで、S16 `8a6efff` は英日 docs、Agent Skill の Codex resume説明、`CHANGES.md` を追加した。
独立 review の起動が safety system の `Potentially unintended activity` で拒否されたため、どちらも独立承認済みとは扱わない。

`8a6efff` の隔離 candidate gate は成功した。

- serial Rust tests: 954 passed、0 failed、4 ignored
- gateway: 70 passed、0 failed
- `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --no-default-features --all-targets`、`git diff --check`: 成功
- 4 ignored は既知の process-boundary tests。S15 時点では direct HTTP upgrade reconnect ignored E2E を明示実行し、1 passedだった。

これは activity candidate 単体の結果である。integration へ取り込んだ最終 tree の全 gate と対応 macOS 実機確認は未実施である。

### 比較評価

- T01-A: 実装・gate済み。独立 review confirmation が未完了。
- T06-B: 実装・parent側再検証済み。独立 review が未完了。
- T05-C: 途中結果を保持して停止。最終 candidate / review は未完了。
- T07-C: 未実行。
- T08-C / T08-A: 未実行。
- T09 / T10: S15 accepted common base が確定していないため未実行。

Codex app-server の停止後 `resume` 検証は、accepted receipt が `Applied`、task が `Interrupted` となった。これは保持済み thread / turn を reconcile し、終了済み child process や新 turn を作らない契約どおりである。一方、`resume` 適用後に `completed` へ到達する live evidence はなく、replacement process PID も直接記録していない。この限定を results に保持する。

### 実 provider / 対応 OS

次の既存4 issue は repository-local 実装とは別に、実 provider、credential、物理 host、対応 OS の evidence を必要とする。

- [live acceptance matrix](20260908-live-acceptance-matrix.md): Cloudflare、OpenAI Secure MCP Tunnel、multi-host、installed runtime の live matrix。
- [client-safe upgrade reconnect](20260908-07-client-safe-upgrade-reconnect.md): macOS process-boundary upgrade reconnect。
- [Codex delegation and app server](20260908-08-codex-delegation-dogfood-and-app-server.md): live smoke、比較評価、採用判断。
- [Vite+ installed Codex runtime](20260911-local-agent-vp-installed-codex-runtime.md): macOS Vite+ launcher と Linux host policy 下の実行確認。

## 目標

- S15 と S16 を immutable diff として独立 reviewし、指摘を同じ branch の別 commitで修正する。
- accepted activity commitをintegrationへ取り込み、その最終 treeで全 gateと明示 process-boundary testsを実行する。
- 未完了の評価 arm と reviewを同じ frozen prompt / base / denominator規則で終え、unknownと失敗を保持した採用判断を記録する。
- 実 provider / macOS / physical multi-host の不足を各既存 issueで検証し、代替 fixtureを実機 evidenceと誤表示しない。
- 権限のある環境からintegration、activity、evaluationの3 branchをpushする。

## 対象外

- push失敗を回避するためのcredential、remote、Git履歴の変更。
- 未確認項目を成功と推測すること。
- 実 provider credential、token、raw transcript、process command lineの記録。

## 提案する方針

1. activity `5be7782` を base `3abf3d1553be64cb59970400d554d834eeb29d20` から独立3-pass reviewする。続けて docs `8a6efff` を reviewする。
2. 指摘解消後の exact commitsをintegrationへ取り込み、private `HOME` / `CODEX_HOME` / XDG / Temote runtime / short socket namespaceで通常 gateと必要な ignored process-boundary E2Eを実行する。
3. evaluation resultsの T01-A / T06-B reviewを閉じ、T05-C、T07-C、T08-C/A、T09/T10をmanifestの順序と比較条件で再開する。
4. app-server resume evidenceは `Applied + Interrupted` を契約どおりの結果として保持し、completed例が必要なら終了していない同一childを使う別の回復可能なtransport pauseで検証する。
5. 既存4 issueの実機・provider項目をそれぞれのevidence要件に従って更新する。
6. 最終commitとgate結果を確認してから、権限のあるcredentialで3 branchをpushする。

## 受け入れ条件

- [ ] S15 / S16 の独立 review結果とexact commit/hashがrepositoryに記録される。
- [ ] activityを含む最終integration treeで通常gateと指定process-boundary E2Eが成功する。
- [x] T01-A、T06-B、T05-C、T07-C、T08-C/A、T09/T10が完了または明示的なfailed / blockedとしてresultsに反映される。（今回はblockedの記録であり、実行完了ではない。）
- [x] app-server resumeの意味と観測限界がresults、英日usage、Agent Skillで一致する。（documentationのみ。新しいlive evidenceなし。）
- [x] 既存4 issueの実 provider / 対応OS / physical host不足が完了するか、具体的な外部条件付きでopenのまま記録される。（今回は具体的な不足条件を記録してopenを維持。）
- [ ] integration、activity、evaluation branchのpush結果が記録される。

## テスト計画

各 owning issue / evaluation manifest の既存 commandを使う。最終integrationではAGENTS.mdの通常gate、gateway test、activity CLI/E2E、明示upgrade reconnect E2Eをprivate test環境で実行する。実 providerとmacOSはfixtureで代替せず、host / provider種別とnon-secretな結果だけを記録する。

## リスク

- 独立 review未完了のactivity commitを統合すると、nonce fence、upgrade lifecycle、privacyの欠陥を見逃し得る。
- 評価の途中失敗やunknownを除外すると比較結果が偏る。
- push未完了中にlocal worktreeを削除するとcommit参照を失うため、remote確認まで上記branchとworktreeを保持する。

## 変更履歴

`CHANGES.md` impact: no（残存作業の追跡だけであり、利用者向け動作は変更しない）。

## 注記

- 2026-09-15: 利用枠による停止時点の実装、review、evaluation、実機確認、pushの残件を集約した。host global state、credential、remoteは変更していない。
