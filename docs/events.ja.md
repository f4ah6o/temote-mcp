# Fabric MCP Events

Modern Fabric MCP エンドポイントは [MCP Events の webhook 契約](https://developers.openai.com/plugins/build/mcp-events)に対応します。初期イベントは `job.state.changed` と `session.state.changed` です。フィルターには `host_id` と `session_id` が必須で、job イベントには `job_id` も指定できます。Legacy MCP は変更しません。cursor は常に `null` で、再送履歴の取得には対応しません。

D1 と専用 sender の認証付きヘルスチェックが成功した場合に限り、client-token 認証の利用者へ Events を公開します。Cloudflare Access JWT 利用者には、配信時の失効確認を行う永続的な認可元がまだないため公開しません。メールの許可リストだけを継続認可として扱いません。

## 実行環境への接続

既存の `OBSERVATION_DB` に `fabric/migrations/0005_mcp_events.sql` を適用します。通常の secret 管理を通じて Worker に以下を設定します。

- `EVENT_SENDER_URL`: Access 保護された Cloudflare Tunnel を通る、`/events/send` で終わる固定 HTTPS URL。
- `EVENT_SENDER_ACCESS_CLIENT_ID` と `EVENT_SENDER_ACCESS_CLIENT_SECRET`: Access service token の secret binding。
- `EVENT_SENDER_BEARER`: 専用 Host sender と共有する、32 文字以上の別のランダムな bearer secret。

専用 Host HTTPS listener に `events_sender::router(bearer)` を組み込み、Tunnel の背後に置きます。sender の経路は認証付きの `/events/health` と `/events/send` だけです。検証または単一イベントの上限付き要求のみを受け取り、DNS の全アドレスを検査して検証済みソケットへ接続します。TLS のホスト名は元の callback 名を保持し、proxy と redirect を無効にします。署名したバイト列をそのまま送ります。汎用リクエスト proxy として使わず、bearer を metadata、log、通常の tool 出力へ出さないでください。

Worker の `scheduled` handler は D1 outbox を処理します。デプロイ側で定期実行を設定してください。遷移を記録した直後にも上限付きの処理を試みます。sender または Host が利用不能な間は、有効期限まで outbox に保持します。callback への実際の失敗は指数バックオフで最大 8 回試行します。HTTP 410 と 413 は終了扱いです。再試行でも event ID は同じで、署名時刻と署名は毎回更新します。既定 TTL は 24 時間、上限は 7 日です。`ttlMs: null` にも有限の 24 時間を返します。secret の更新後は 5 分間二重署名します。

## 正本の状態遷移

Fabric は、認証済み Host 経路から受けた `session_info`、`job_list`、`poll_job` の応答を観測します。これらの読み取りなしでも遷移を速やかに送るには、Host 側の正本の遷移処理から、通常の federated Host 認証を使い `/v1/hosts/{host_id}/events/transition` へ送信します。本文の形式は英語版 [events.md](events.md) を参照してください。`session.state.changed` では `job_id` を省略します。

gateway は Host の generation と instance を検査し、保持された session の process ID と canonical scope が現在の `session_info` と一致することを確認します。正本が欠ける、または degraded な場合は 409 を返し、終了状態を捏造しません。同じ Host/session ID を再利用した別インスタンスには、前インスタンスの outbox を配信しません。Host は job 完了を実際の状態遷移経路から送る必要があります。Fabric はイベント生成だけを目的とする polling loop を作りません。

外部認証情報を使う ChatGPT Work の callback・反応 E2E は **NOT RUN** です。Access JWT 利用者へ Events を公開する前には、Access principal の失効を再検査できる仕組みも必要です。
