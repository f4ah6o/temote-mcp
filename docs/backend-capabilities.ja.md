# バックエンドのタスク機能

Temote MCP は許可済みセッション内で型付きタスクを受け付けます。タスクツールにコマンド、実行ファイルのパス、環境変数、ネットワークポリシーを渡すことはできません。実行状態の `completed` は検証の PASS や配信の完了を意味しません。

| バックエンド | 構造化レポート | タスク表示のソース | 上限 |
| --- | --- | --- | --- |
| Codex app-server 0.157.1 | `turn/start.outputSchema` で最終メッセージを制約し、Temote が解析後に検証 | `native_structured_output` | JSON 8 KiB |
| OpenCode serve v2.0.11 | V2 prompt の `format: {type: "json_schema", schema}` を使い、assistant message の `structured_output` を検証 | `native_structured_output` | JSON 8 KiB |
| OpenCode 旧 SDK | 検証済みの native format 経路なし。最終メッセージの JSON を上限付きで抽出 | `final_message_compat` | JSON 8 KiB |
| Devin ACP | schema 制約付き prompt/result 経路は未確認。最終メッセージの JSON を上限付きで抽出 | `final_message_compat` | JSON 8 KiB |
| Devin Cloud | `structured_output_schema` と必須の構造化出力 | `native_structured_output` | 既存の Cloud 応答・レポート上限 |

共通レポート契約には二つの互換プロファイルがあります。Delegation の証跡ファイルは、要求・観測モデルと effort を含む十フィールドを厳密に要求します。TaskReport は `status` と `summary` を要求し、配列は任意で、従来のシリアライズ済み 8 KiB 上限を維持します。Devin Cloud のワイヤスキーマのバイト列は変更しません。native 結果が不正または欠落している場合、最終メッセージで成功扱いに上書きしません。

Codex の `outputSchema` はインストール済み 0.157.1 の `v2/TurnStartParams` スキーマで確認しました。OpenCode の request と `structured_output` 応答は [OpenCode SDK の構造化出力契約](https://opencode.ai/docs/sdk/#structured-output) に従います。モデル・プロバイダーでの受理はホストでの live 確認が必要です。

Codex の完了・中断済みタスクへの `steer` は同じタスク ID で新しい turn を開始します。generation と turn ID を更新し、照合中は直前の turn を排除します。実行中の turn への `steer` は従来の `turn/steer` を使います。operation ID は永続的な再実行キーです。ホスト管理者は `TEMOTE_CODEX_BINARY` で実行ファイルを選べます。旧名 `TEMOTE_MCP_CODEX_BINARY` は読み取り専用の互換別名です。タスク引数からは選べません。

OpenCode はタスク受理前に serve 実行ファイルとセッション範囲の書き込み可否を確認します。repository/workspace の要求がない場合、checkout の欠落は助言です。native shell 権限は拒否したままです。Host が opt-in した managed task は、private な型付き [workspace command bridge](opencode-scoped-workspace.md) を利用します。通常の task は shell 拒否を維持します。provider を使う managed build/test の受け入れ検証は別途 NOT RUN として記録します。preflight は checkout を作成・変更しません。

プロバイダーを使う構造化出力とローカル socket/process のライフサイクルテストは、非サンドボックスのホストまたは CI で確認する必要があります。ローカルの決定的 fixture はスキーマ、解析、タスクの冪等性を確認しますが、すべてのモデルでのプロバイダー対応を保証しません。

## Codex 会話の継続

`codex_task_start` の任意の `continuation` に `{"type":"previous_task","task_id":"<UUID>"}` を指定すると、新しい Temote タスクで以前の Codex 会話を継続できます。元タスクは、同じアクティブなセッションと正規化された範囲に保持された Codex タスクであり、terminal 状態、進行中の操作なし、保持された thread ID あり、という条件を満たす必要があります。Temote は後継タスクを一つだけ確定し、元の runtime を終了してから、新しい app-server で `thread/resume` と新しい `turn/start` を行います。後継タスクの ID、receipt、実行、検証、delivery は独立しています。task view の `continued_from_task_id` と `continued_by_task_id` で関係を確認できます。省略時または `{"type":"new"}` は新しい thread を開始します。ローカル CLI の Codex start では `--continue-from-task <UUID>` を使用します。不確かな start は同じ `operation_id` と同一内容で再試行してください。

旧 `delegate` / `codex exec` / `opencode run` 経路は互換期間中も利用できます。今回の server 検証を一対一の live parity と同一視せず、未実行の各測定と残した互換条件は [parity 一覧](backend-capabilities.md#legacy-entrypoint-parity) に記録しています。
