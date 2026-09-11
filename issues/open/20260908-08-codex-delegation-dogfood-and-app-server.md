# TEMOTE-08: Lunaへの実装委譲を今回の開発で実証し、Codex app server連携へ進める

- Status: Open / implementation and fake-transport verification landed; one real `codex delegate` dogfood task recorded 2026-09-11; real app-server dogfood and comparative measurement remain
- Date: 2026-09-08 (Asia/Tokyo)
- Updated: 2026-09-11 (Asia/Tokyo)
- Priority: P1
- Original baseline inspected: `8b95f1d91734e77e698d55d2babd097860f1c1b3` (`main`)
- Implementation baseline now in `main`: `da64a3800eafb9ef19237ebd9d7310502201873f` and follow-up commits
- Requested outcome: この機能の開発自体で、Luna Maxへ調査・実装・テストを任せ、親モデルが短い報告と証拠をレビューする。
- Related:
  - [TEMOTE-05: durable continuation / apply_patch](../done/20260908-05-durable-continuation-and-apply-patch.md)
  - [TEMOTE-06: friction / learning / recall](20260908-06-friction-learning-recall.md)
  - [TEMOTE-07: client-safe upgrade / reconnect](20260908-07-client-safe-upgrade-reconnect.md)
  - [`docs/evaluations/codex-delegation.md`](../../docs/evaluations/codex-delegation.md)

## 現在の状態（2026-09-11）

この issue の実装部分はすでに `main` に入っている。`codex delegate` の bounded structured report、scoped evidence、experimental app-server task controls、gateway contract、fake-transport/権限/失敗系テストが実装済みである。

2026-09-11 に macOS 開発ホストで Vite+ 管理の Codex CLI `0.153.4` を使い、実 `codex delegate` を小タスクで1件実行した。`status=success`、exit 0、requested `gpt-5.6-luna`/`high`、observed model/effort は child event 非公開のため `null`、usage は入力 50442 / cached 32256 / 出力 512 / reasoning 151、worktree で `hello.txt` 作成を確認した。non-secret の記録は `docs/evaluations/codex-delegation.md` に保存している。app-server の live 実行と比較測定は未完了。

同じ Vite+ 管理 Codex は `local_agent_run` の local-agent sandbox では launcher 依存 path 可視性の問題で起動できない。これは `20260911-local-agent-vp-installed-codex-runtime.md` で別途追跡しており、`codex delegate` の live 結果とは独立である。

一方、実 Codex dogfood と効果測定は未完了。repository 内の評価記録では、対象環境の Codex vendor binary 欠落により real Luna Max task、real app-server model listing、authentication success、observed usage を確認できていない。

したがって、この issue の残作業は **新しい delegation 実装ではなく live dogfood / measurement / adoption decision** である。以下の historical phase checklist の unchecked 項目を機械的に再実装しない。現行コードと `docs/evaluations/codex-delegation.md` を先に確認する。

### 次に着手する最小スライス

1. 検証済み Codex build が利用できるホストで、要求 model/effort を確認する。
2. 同じ小規模 repository task を `codex delegate` で1件実行し、worktree/diff/test と structured report を照合する。
3. 同じ条件で app-server path を1件実行し、resume/control/usage/status evidence を確認する。
4. 同一 base/permissions/acceptance criteria で direct Temote / `codex exec` / app-server の比較を記録する。
5. 結果を `docs/evaluations/codex-delegation.md` に追記し、採用・条件付き採用・不採用を決める。live evidence で実装 defect が出た場合だけ、その defect を別の小さい修正単位として扱う。

## 目的と判断

親モデルがTemoteでファイル取得・編集・コマンド実行・結果確認を細かく繰り返す作業を、ホスト上のCodexへタスク単位で委譲する。第一候補は `gpt-5.6-luna` / reasoning effort `max`。親は目的・制約・受入条件を定義し、例外判断と成果物のレビューを行う。

節約効果の本体は、実装ループとその文脈を子側に置くこと。app serverの追加価値は、継続、実行中の追加指示、中断、承認、状態・使用量の取得にある。親子合計のトークン・費用・時間や品質が改善することは、現時点では未検証。

今回の実装は「親が全部実装してから委譲を試す」という進め方にしない。まず既存の `codex exec` で最小の委譲経路を用意し、その経路で後続の実装を進める。効果・品質・権限のゲートを満たしたらapp serverアダプターへ進む。

## 現行コードから確認したこと

確認元: baselineの `src/mcp.rs`, `src/sandbox.rs`, `src/codex.rs`, `docs/usage.md`。

- `read_file` は最大8 MiBの全文取得で、行範囲の引数がない。
- コマンドのstdout/stderrの保持上限は合計1 MiB。返却量を呼出側が小さく指定する引数がない。
- 完了済みjobのpollはキャッシュ済み結果を再返却する。短い状態照会と結果取得を区別する余地がある。
- `src/codex.rs` はCodexプラグインの導入・削除・状態・診断を担当し、Codexタスク実行は担当していない。
- job / checkpoint / handoff / frictionには再利用可能な基盤がある。ただし通常jobはin-memoryで、session停止・寿命により終了するため、永続タスクや実行中処理の再接続を保証するものではない。

これらは消費を増やし得る要因であり、毎回上限量がモデルに入力されるという意味ではない。MCP返却バイト数を実際のモデル入力トークン数や利用枠消費と同一視しない。

## 実装担当への開始指示

1. 現在のworktree、`git status`、`git diff`、HEAD、AGENTS.md、本issueと関連コードを確認する。baselineや過去handoffを現在状態と扱わない。
2. 既存変更を保持し、実装用worktreeを分離する。実行中のTemoteランタイムを再起動・更新しない。
3. 対象ホストのCodexバージョン・認証の利用可否・実際のモデル/effort対応を確認する。認証情報本文は読み出さない。可能ならapp serverの `model/list` で `gpt-5.6-luna` と `max` / `high` を確認する。
4. Lunaが利用不能なら、阻害要因と利用可能な選択肢を記録する。黙ってモデルを置換し、Lunaで実証したことにしない。
5. 稼働中Temoteで権限上許される既存実行経路を使い、Phase Aから開始する。通常sessionのnetwork禁止を迂回しない。
6. 実装中に実際の拒否・権限不足が発生した場合は、その操作と返答を記録する。別経路のCodex実行で拒否された操作を再実行しない。

## 今回の開発で使う委譲プロセス

### 親と子の責務

親はタスクの範囲、開始コミット、許可された作業場所、制約、合格条件、必要な証拠を渡す。会話履歴全文や既読コード全文を渡さず、issue・対象ファイル・関連知見の参照を使う。

子のLuna Maxは自身で必要箇所を調査し、実装、関連テスト、失敗修正まで担当する。親へ逐一コマンド選択を問い合わせない。仕様変更、権限境界の変更、再現不能な失敗等は短い質問として返す。

親は報告と差分・テスト証拠を照合し、未達箇所を同じ子threadへ返す。修正を親が引き取った場合は、その理由、時間、変更量も評価に含める。親による全面再調査・全面再実装を通常フローにしない。

### タスク入力

- `task_id`、UUID `operation_id`、親タスク参照
- repo / canonical worktree scope / base commit / 許可された変更範囲
- 目的、非対象、制約、受入条件、参照ファイル
- 要求model / effort、許可された操作と既存の承認条件
- 報告サイズ上限、実行上限、エスカレーション条件

自由文のタスク指示は権限付与として扱わず、実行権限は別の構造化ポリシーで強制する。

### 報告契約

既定の親向け報告はUTF-8で4 KiB以内、スキーマ検証付きとする。長い一覧やログは参照へ分離し、上限超過を明示する。以下を含める。

- 完了/失敗/中断/判断待ち等の状態、短い変更概要
- base commit、変更ファイル一覧の参照、diffまたはcommitへの参照
- 実行したチェック、結果、対象tree/diffの識別子、証拠参照
- 未実施チェック、未解決事項、親への質問
- 要求model / effortと観測できた実際のmodel / effort、使用量、測定の出所

子の「成功」という文章だけで検証済みにしない。プロセス終了コード、構造化イベント、テスト証拠と子の自己申告を分離する。JSON Schema準拠は報告の真実性を保証しない。

同一の失敗が2回続いた場合は再試行を続ける前に、仮説・試行差分・必要な判断を親へ報告する。タスク分割や範囲修正で続行できる場合は続行し、未完了を成功扱いしない。

## Phase A: codex execで最小経路を作り、その経路で開発を始める

- [ ] Codexバージョンを記録し、その版のCLIヘルプ/公式仕様に合わせて起動引数を確定する。
- [ ] 親が作るbootstrapは、必要最小限の起動・JSONL収集・終了判定・報告スキーマ検証に限定する。bootstrap自身の費用も測る。
- [ ] `codex exec --json` と `--output-schema` / `--output-last-message` を用い、全JSONLやstderrを親へそのまま返さない。
- [ ] 構造化最終報告とusageを抽出し、失敗、報告欠落、不正JSON、子プロセス終了を区別する。
- [ ] 正常系の小さい実タスクを1件、Luna Maxで調査→実装→テスト→報告→親レビューまで完了させる。
- [ ] 次にPhase Bの実装単位をLuna Maxへ委譲する。委譲経路の最初の実績を今回の開発に残す。

通常Temote `execute` はnetwork禁止のため、Codex API通信を単純な子プロセスとして動かせると仮定しない。bootstrapでは既に許可されたホスト実行経路とCodex自身の権限制御を使い、有効な権限を記録する。新しい通常session向け実行経路はPhase Cで設計・検証する。

Phase Aを後の本番APIとして固定する必要はない。測定・報告・タスク契約を再利用し、外部シェル実行の汎用口を増やさない。

## Phase B: 出力量の改善と測定をLunaへ実装委譲

- [ ] `read_file` に後方互換な行範囲・返却バイト数上限を追加する。UTF-8境界、切詰め、次の取得位置を明示する。
- [ ] コマンド結果に小さい返却上限と状態のみ取得する選択肢を設ける。既存の全結果取得・retry契約を壊さない。
- [ ] 長い結果の参照をsession/scopeに束縛する。必要な範囲だけ取得でき、任意ファイル読み出しにはならない。
- [ ] task単位でMCP呼出回数、親へ返したバイト数、所要時間、再試行、親の修正量を記録する。
- [ ] Codex usageの入力・cached input・出力・取得できるreasoning内訳を記録する。合計と内訳を二重加算しない。
- [ ] 親モデルusageはクライアントから取得できた実測のみ使用する。Temoteだけで取得できない場合は `unknown` とし、バイト数による推定は別欄にする。
- [ ] gateway契約・テスト・usageドキュメントを更新する。

Phase Bの入出力改善とモデル委譲の効果が混ざらないよう、比較実験では親の直接操作群にも同じI/O改善を適用する。改善前との比較は別の測定として記録する。

## Phase C: app serverアダプターをLunaへ実装委譲

着手条件はPhase Aの実タスク合格、Phase Bの観測可能性、以下の権限設計の親レビュー合格。節約効果の正式判定はPhase Dで行う。

### 小さい公開surface

具体名は既存命名に合わせて調整可能だが、責務は維持する。

- `codex_status(session_id)`: 利用可否、版、対応model/effort、実行上の制約。認証情報本文を返さない。
- `codex_task_start(session_id, operation_id, task, model, effort, ...)`: 検証済みタスクを受理し、短いtask handleを返す。
- `codex_task_get(session_id, task_id, after_revision?)`: 状態変化、usage、上限付き報告、証拠参照。変化なしは短く返す。
- `codex_task_control(session_id, task_id, operation_id, action, ...)`: 型付きのsteer/resume/interrupt。任意JSON-RPC転送は公開しない。

証拠取得にはscope付きの既存/Phase Bの読取手段を再利用する。承認は既存のローカル承認経路に接続し、通常のMCP task操作だけで承認済みにしない。

### transport・タスク寿命・再試行

- [ ] Temoteがホスト上で管理するapp serverにローカルstdioで接続する。app server自体のpublic listenerは追加しない。
- [ ] 導入版に対応するschemaと互換性テストを用意し、未対応method/fieldは明示的に拒否する。
- [ ] `initialize`、thread start/resume/read、turn start/steer/interrupt、通知、承認要求を扱う。
- [ ] taskをcaller ownership、session、canonical scope、thread ID、turn ID、実行世代へ束縛する。session IDだけを認証根拠にしない。
- [ ] 同じoperation IDと同じ正規化要求は同じ受理結果を返す。異なる要求はconflict。受理記録を副作用前に永続化する。
- [ ] app serverの受理とTemoteへの記録の間で落ちた場合、JSON-RPC request IDだけで実行の冪等性が保証されるとは扱わない。既存thread/turnを照会し、確認不能なら `unknown` / reconciliation required。盲目的に再startしない。
- [ ] MCP接続の喪失とsession停止・プロセス停止を分離する。接続だけ失われた場合は同じタスクを照会する。
- [ ] session停止時の子プロセス終了、孤児防止、実行期限、保持期限を明示する。既存jobのin-memory状態だけに依存しない。
- [ ] thread履歴のresumeと、中断されたOSプロセスの復活を区別する。再起動後に実行中処理が戻るとは保証しない。
- [ ] pending approval、failed、interrupted、unknownをcompletedと区別し、報告のrevision/cursorで重複転送を抑える。

### 権限と保存

- [ ] 通常sessionのpermitted roots、symlink検証、Git metadata保護、承認を子にも実効的に適用する。単なるcwd指定やプロンプト制約では不足。
- [ ] Codexの推論用通信と、生成されたshell/testコマンドの通信権限を分ける。通常executeのnetwork禁止やpublic HTTPの制約を緩めない。
- [ ] host integrationとしての起動承認、Codexからの操作承認、Temoteのsession権限の対応表を実装前に記録する。自動approve-allを実装しない。
- [ ] yoloをCodex側の全権限へ黙って変換しない。Codex/クライアント独自の承認条件を維持する。
- [ ] 子の同じTemote委譲APIへの再帰呼出しを防ぎ、通常のローカルファイル操作を利用する。並列化は今回の必須範囲に含めない。
- [ ] 認証はホスト上の既存Codex認証を利用し、credential/環境変数を親へ渡さない。
- [ ] task metadata / audit / frictionには識別子・enum・数値を中心に保存し、prompt、transcript、raw output、秘密を入れない。
- [ ] 詳細ログはmetadataと別のowner-only領域で、サイズ/期限/取得範囲を制限する。ログに機密が入り得るため、自動Git追加・外部公開・親への全文返却をしない。Codex自身の履歴保存も確認する。

これらの境界が実効的に保証できない場合、通常session対応を完了扱いしない。制約を明示したopt-in実証までに留め、必要な設計変更を記録する。

## Phase D: 同じ開始条件で効果検証

### 比較設計

- [ ] 今回の実装から正常系・失敗修正・境界テスト・文書/契約更新を含む実タスクを約10件選ぶ。
- [ ] タスクごとに同じbase commitの独立worktreeを使い、同じ指示、受入条件、I/O機能、親model/effort、権限、環境で比較する。
- [ ] A: 親が直接Temoteを操作、B: Luna Maxへ委譲、C: Luna highへ委譲。
- [ ] 実行順を交替し、他群の解答・差分・完了報告を入力に混ぜない。各群は新しいthreadで開始する。
- [ ] 親レビュー時には可能な限り実行群を伏せ、同じ合格基準を使う。
- [ ] 失敗・親の救済・再試行・中断を除外せず集計する。結果が揺れるタスクは追加反復する。
- [ ] 最終製品には受け入れた1つの変更だけを統合し、比較用worktreeの重複差分を混ぜない。
- [ ] 独立した比較環境で両アダプターを利用できる状態からexec対app serverも比較する。開発前後の単純比較だけでapp serverの節約効果と断定しない。

この比較は少数タスクのpilotであり、統計的な一般保証とは扱わない。自分の実装でのdogfood記録と、揃えた条件の比較結果は別表にする。

### 必須測定項目

| 項目 | 扱い |
|---|---|
| 親のinput/cached input/output/reasoning | 取得できた実測。取得不能はunknown |
| 子の同内訳、要求/実際のmodel・effort | Codexイベント由来。rerouteや欠落を明示 |
| 親子合計tokenと費用 | cache・reasoning内訳の二重計上禁止。失敗とレビューも含める |
| MCP往復・返却バイト数 | 親のtokenの代用品と断定しない |
| end-to-end時間、子の時間、親の介入時間 | 起動、待機、レビュー、再実行を含める |
| 初回合格率・最終合格率 | 未完了を除外しない |
| 親が修正した回数・行数・理由 | 大きいモデルによる救済コストを可視化 |
| 権限・停止・再接続の失敗 | token改善と独立した必須品質ゲート |

API費用は測定日の公式単価、課金条件、認証方式を記録して算出する。ChatGPT定額利用の利用枠は別指標とし、API価格換算を実際の節約額と表示しない。観測不能な総費用について削減率を発表しない。

### 事前に固定する暫定採用基準

結果を見てから有利な閾値へ変更しない。以下は目標であり、予測値ではない。

- 親の実測token中央値が直接操作比30%以上減る。
- 測定可能な場合、親子合計API費用中央値が20%以上減る。定額環境ではこの条件をAPI換算で代用せず、利用枠の実測と判断理由を残す。
- 最終合格率が直接操作群を下回らず、重大な見落とし・権限違反がない。
- end-to-end時間中央値が直接操作群の1.25倍以内。超過する場合は用途を限定する根拠を明記する。
- 親の全面書き直しを成功と数えず、救済を含めた費用/時間で判断する。
- max対highでmaxの品質利益が確認できなければ、既定effortは実測に基づき決定する。Luna Maxを評価した事実は保持する。

親usageや費用が観測不能なら、その条件は未判定。報告量削減・品質・継続操作の価値を限定的に結論付け、コスト削減の正式合格とはしない。

## Phase E: 統合・運用文書

- [ ] 合格した実装を段階的に統合し、Codex依存のない既存Temote機能は引き続き動作する。
- [ ] `docs/usage.md` / `docs/usage.ja.md` にタスク操作、権限、保存、失敗/再開、測定上の制限を記載する。
- [ ] 運用手順が変わる範囲だけ `skills/temote-mcp/SKILL.md` を更新する。READMEは短く維持する。
- [ ] 評価手順・集計・採用判断を `docs/evaluations/codex-delegation.md` に残す。raw transcriptや機密ログはコミットしない。
- [ ] TEMOTE-06への接続は既存のsecret-free enum/countに限定し、子の自己申告をobservedに格上げしない。
- [ ] TEMOTE-07のupgrade/reconnect実装を今回の前提にしない。実証のため稼働中Temoteを更新しない。

## 受入テストと完了条件

- [ ] 今回の実装で、Luna Maxが少なくとも1つの実装単位を調査→実装→テストまで担当し、親が証拠をレビューした記録がある。
- [ ] 同じoperation_idのretry、異なる内容のconflict、受理直後の応答喪失で二重実行しない。
- [ ] 実行状態が不明なcrash、接続切断、再照会、session停止、子の異常終了で正しい状態を返す。
- [ ] pending approvalと拒否を正しく扱い、別session/scopeからtaskや証拠へアクセスできない。
- [ ] 通常sessionでroot escape、symlink、Git metadata書込、子コマンドのnetwork権限拡大を防ぐ。
- [ ] 巨大出力、UTF-8境界、不正JSON、報告欠落、秘密のcanary、usage欠落・重複通知を検証する。
- [ ] 親が通常取得する応答のサイズ上限を強制し、状態照会で全文履歴を返さない。
- [ ] exec/app serverの差し替えでtask/report/usage契約を保持する。実Codexの小規模smokeとfake transportの障害テストを分ける。
- [ ] Phase Dの結果、失敗、unknown、採用/条件付き/不採用の判断を記録する。
- [ ] 実装時のAGENTS.mdに従いformat、Rust tests、clippy、no-default-features、gateway tests、diff checkを通す。

「APIが動いた」「子が完了と言った」だけでは本issueをdoneにしない。今回の委譲プロセスの実行証拠、品質確認、比較評価、採用判断を揃える。効果が不足した場合も結果を隠さず、exec運用の維持・app serverの用途限定・既定effort見直しのいずれかを理由付きで選ぶ。

## 公式資料

参照確認日: 2026-09-08 (Asia/Tokyo)。実装時は対象CLI版で再確認する。

- Codex app server: https://developers.openai.com/codex/app-server
- Non-interactive mode / JSONL / structured outputs: https://developers.openai.com/codex/noninteractive
- GPT-5.6 Luna: https://developers.openai.com/api/docs/models/gpt-5.6-luna

公開仕様でのモデル対応と、対象ホスト・認証方式での利用可否を混同しない。app serverの実験的APIやtransportの注意は導入版ごとに確認し、未検証の本番互換性を保証しない。
