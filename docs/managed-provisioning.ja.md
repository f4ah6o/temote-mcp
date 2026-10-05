# Managed repository provisioning

認証済み `session_start` は `path` と `source` のどちらか一方だけを受け付けます。`path` は既存の named-root workspace 方式です。`source` は生成された workspace を準備し、呼び出し側が生成した UUID `operation_id` が必須です。

```json
{"source":{"kind":"repository","repository":"f4ah6o/temote-mcp","base":"main","vcs":"auto"},"operation_id":"d850ca8e-3ec7-43ef-8b75-e8c889f9e126"}
```

`source` に repository 文字列だけを渡すと `auto` と `main` を選びます。supervisor に `TEMOTE_ROOTS` を設定してください。named root が複数ある場合は `TEMOTE_WORKSPACE_ROOT` に選択する root 名を指定します。`TEMOTE_MCP_ROOTS` と `TEMOTE_MCP_WORKSPACE_ROOT` は読み取り専用の互換 alias で、canonical 名が優先されます。隔離した local socket の `TEMOTE_SOCKET_NAMESPACE` も `TEMOTE_MCP_SOCKET_NAMESPACE` より優先されます。

最初に受理した要求は、repository identity、root、生成された SessionId・WorkspaceId・ChangeId、委譲 task の operation ID を準備前に保存します。同じ `operation_id` と同じ要求で再試行して進捗を確認します。内容を変えた再試行は conflict です。応答を失った場合も同じ ID を維持してください。pending は active session を意味しません。repository と environment の各 task の完了確認には同一要求の再試行が必要な場合があります。environment task は別の operation UUID、model、effort、入力 digest を委譲前に保存します。

自動準備は Codex が公開する表示対象の既定モデルと、対応する既定 reasoning effort を選びます。既定情報のない古い一覧は掲載順を維持し、hidden モデルは除外します。複数の既定モデルがある場合は停止します。選択結果は委譲前に receipt に保存し、再試行でも維持します。必要な HTTPS fetch は子 agent のネットワーク承認に従い、BLOCKED report や ready marker の欠落を workspace の準備完了として扱いません。

host は選択した named root の下に bare Git store と独立 workspace を配置します。委譲された Codex agent が一時的な root-scoped normal session 内で fetch と workspace 作成を実行します。allocation marker は operation、repository、workspace、change、固定 commit、backend を結び付けます。認識できる Cargo または pnpm の manifest がある場合、同じ root-scoped agent が依存関係を隔離して準備し、別の environment ready marker を書きます。Temote は両 marker と準備 session の完全な owner identity を検証してから workspace session を起動します。対応外または曖昧な manifest は `environment_unsupported` を返し、agent-ready session を起動しません。local `main` checkout は作らず、既存 checkout を変更しません。

`auto` と `jujutsu` は `jj` を必要とし、未導入時に Git へ暗黙に切り替えません。`git` は明示的な互換 mode です。workspace はその内部に独立した実 directory の `.git` を持ち、外部 common directory を参照できません。呼び出し側から host path、argv、environment、network policy は指定できません。

receipt は受理時の canonical root と、scope・permission mode を含む起動済み session の完全な instance identity を保存します。root が変わった場合や停止・置換された instance は replay で採用しません。停止済みは停止のままで、証明できない instance は `unknown` または `reconciliation_required` を返します。Codex task の受理後、receipt の task ID 更新前に応答を失った場合、同じ task operation UUID・prompt・model・effort で retained Codex receipt を読み、task ID を回復します。backend receipt が見つからないか不確かな場合は reconciliation のままとし、新しい準備 task を盲目的に開始しません。
