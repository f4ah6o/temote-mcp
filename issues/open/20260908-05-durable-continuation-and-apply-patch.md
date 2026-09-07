# TEMOTE-05: durable continuation hardening と Codex-style apply_patch

- Status: Open
- Date: 2026-09-08 (Asia/Tokyo)
- Priority: P1
- Baseline: `89a7981a7e566643a5502716a11085801cd44422` (`main`)
- Depends on:
  - [TEMOTE-01](20260905-01-session-job-discovery.md)
  - [TEMOTE-02](20260905-02-scoped-task-checkpoints.md)
  - [TEMOTE-03](20260905-03-read-only-work-handoff.md)
- Reference implementation / design study:
  - https://github.com/totec448-spec/chat-on-steroids
  - `src/main/session/continuation.ts`
  - `test/continuation.test.ts`
  - `docs/tool-surface.md`
  - `src/main/codex/apply-patch/`
  - `src/main/codex/tool-specs.ts`

## 目的

TEMOTE-01/02/03 を、会話やMCP transportの切断・応答消失・client再試行が起きても二重実行や誤った再開を起こしにくい durable continuation 基盤として完成させる。

あわせて coding agent が既知の contract で安全に複数ファイルを編集できる `apply_patch` primitive を追加する。

Chat On Steroids (COS) の browser / ChatGPT orchestration 自体を移植するのではなく、以下の設計原則だけを Temote の既存 session / sandbox / approval model に合わせて採用する。

1. durable identity は ChatGPT conversation ではなく Temote `session_id` / scoped work state とする。
2. side effect の前後を durable checkpoint で区別し、結果が不明な状態を `failed` と決めつけない。
3. client retry を前提に mutating operation を idempotent にする。
4. commit は preflight → durable write → publish の順で行い、途中失敗時は以前の authoritative state を保持する。
5. crash boundary を deterministic test で直接検証する。
6. file edit は全対象を preflight してから mutation を開始する。

## 非目標

以下は Temote core へ取り込まない。

- Chrome extension / ChatGPT DOM 操作。
- ChatGPT conversation ID を Temote session の authoritative identity にすること。
- worker chat / swarm / `agents` / Goal / Loop。
- browser tab lifecycle 管理。
- COS の unrestricted host shell semantics。
- 会話全文、command argv、stdout/stderr、tool result 全文の恒久保存。
- OpenAI 側 safety review / tool blocking の迂回。

Temote の normal session にある sandbox、network restriction、permitted roots、local approval、secret非永続化は維持する。

---

## Phase A: TEMOTE-02 mutation の idempotency / ambiguity 強化

### 問題

現状の TEMOTE-02 設計では新規 checkpoint は server が UUID を発行する。

次の failure では同じ client intent から複数 checkpoint が作られ得る。

```text
checkpoint_save(create)
  -> disk commit 成功
  -> MCP response 消失
  -> client が同じ create を retry
  -> 別 UUID で2件目が作られる
```

これは transport failure と application failure を区別できていない。

### 追加契約

`checkpoint_save` に caller-supplied `operation_id` を必須追加することを第一候補とする。

- UUID v4 など bounded opaque ID。
- 同一 scope + 同一 operation_id + 同一 canonical request は以前の成功結果を返す。
- 同一 operation_id で内容が異なる場合は `OPERATION_CONFLICT`。
- response loss 後の exact retry で新しい checkpoint/revisionを作らない。
- operation record は checkpoint state と同じ durability boundary で確定させる。
- operation record に自由文、secret、command output は保存しない。

mutation lifecycle は少なくとも内部的に次を区別できること。

```text
not_started
committed
ambiguous
failed_before_commit
```

`ambiguous` は「external/host side effect が起きたか local state から証明できない」場合の予約状態とする。
TEMOTE-02 の純粋な local atomic write は原則 `committed` または `failed_before_commit` まで証明できる設計を目指し、不必要に ambiguous を作らない。

将来 Git / integration mutation 等へ同じ operation model を適用できるよう、idempotency helper を checkpoint 固有コードへ過度に埋め込まない。

### crash consistency

checkpoint create/update は以下を満たす。

1. validation / scope / revision / approval を全て preflight。
2. durable temp write + sync。
3. checkpoint と idempotency result の authoritative transition を矛盾なく確定。
4. publish 後の応答消失でも exact retry は同じ result を返す。
5. crash recovery が未commit requestを成功扱いしない。

既存 TEMOTE-02 の `client_reported` semantics は変更しない。
idempotent retry に成功しても reported verification を live verification に昇格させない。

---

## Phase B: TEMOTE-01/03 continuation semantics の明確化

TEMOTE-01 `job_list` と TEMOTE-03 `work_handoff` は、再開時に自動で side effect を再送しない現在の方針を維持する。

`work_handoff` には必要に応じて、後続clientが安全に判断できる bounded metadata を追加する。

候補:

- checkpoint revision / operation identity。
- current live job snapshot。
- `freshness=not_revalidated`。
- reported state と observed state の明示的区別。
- ambiguous operation が存在する場合の `resume_hints`。

ただし handoff text から command を生成・実行しない。
空の job list を「未実行」や「再実行して安全」の証拠にしない。

Temote側の durable identity は常に session/scoped checkpoint とし、ChatGPT conversationの入れ替わりを server state migration として扱わない。

---

## Phase C: `apply_patch` tool

### 目的

`write_file` 全置換だけでなく、coding model が既知の patch contract で複数ファイルをまとめて安全に変更できる primitive を提供する。

COS の Codex-compatible surface を参考にするが、Temote の permission/sandbox model を優先する。

### contract

第一候補:

```text
apply_patch({session_id, patch})
```

- MCP transport 上は JSON string field `patch` を使用。
- Codex/V4A style の `*** Begin Patch` / `*** End Patch` grammar を採用候補とする。
- add / update / move / delete file を明示的にparseする。
- unknown/unsupported construct は mutation 前に拒否。
- patch size、file count、path bytes、per-file/output size を bounded にする。

### security invariants

patch parser が shell を起動しないこと。

全 target について mutation 前に:

1. sessionをloadしてcurrent canonical cwdを確認。
2. source/destination pathをcanonical/lexicalに検証。
3. permitted root escape、`..`、symlink escapeを拒否。
4. operationごとの permission / approval を確認。
5. existing file identity / expected preimage を確認。
6. 全fileのpreflight成功後だけ write phaseへ進む。

multi-file patch の途中で validation failure が判明する設計は禁止する。
可能なら同一 patch の file writes は transactional staging を使い、途中I/O失敗時に partial edit を最小化またはrollbackする。
完全atomicを保証できない場合は contract/docs で明示し、どのfileまでcommitされたかを機械可読に返す。

`apply_patch` を追加しても `execute` を unrestricted shell に変更しない。
normal session の command sandbox/network restrictionも変更しない。

### tool annotations

原則:

- `readOnlyHint=false`
- `destructiveHint=true`
- `idempotentHint=false`
- `openWorldHint=false`

ただし exact same patch retry の server-side idempotency を追加する場合は semantics を別途設計し、annotationだけ先にtrueへ変更しない。

---

## Phase D: crash-boundary / retry tests

happy path より failure boundary を優先してテストする。

必須ケース:

### checkpoint / continuation

- `checkpoint_create_response_loss_exact_retry_returns_original_result`
- `checkpoint_operation_id_conflict_rejects_different_payload`
- `checkpoint_crash_before_durable_commit_does_not_publish_success`
- `checkpoint_crash_after_commit_before_response_is_recoverable`
- `handoff_does_not_replay_ambiguous_or_reported_work`
- `handoff_keeps_reported_and_live_state_distinct`
- `job_list_after_new_chat_discovers_existing_running_job`

### apply_patch

- `apply_patch_preflights_all_files_before_first_write`
- `apply_patch_rejects_symlink_escape`
- `apply_patch_rejects_move_destination_outside_root`
- `apply_patch_denied_approval_writes_nothing`
- `apply_patch_malformed_multi_file_patch_writes_nothing`
- `apply_patch_partial_io_failure_reports_exact_commit_state`
- `apply_patch_does_not_invoke_shell`
- `apply_patch_does_not_widen_session_permissions`
- `apply_patch_secret_content_is_not_copied_into_audit_metadata`

テストで arbitrary sleep / polling timing に依存しない。
commit直前、durable write直後、publish直前などに test-only fault gate を置ける構造を優先し、狙った crash boundary を deterministic に再現する。

---

## gateway / protocol parity

新規/変更toolは Rust MCP surface と gateway contract を同時に更新する。

- schema / required fields / `additionalProperties=false` parity。
- annotations parity。
- public pathでも `session_id` ownershipを失わない。
- gateway が operation_id を生成・書き換えない。
- response truncationが idempotency result の意味を変えない。

既存 `routed-tools.json` parity test を維持する。

---

## docs / attribution

`docs/usage.md` / `docs/usage.ja.md` に以下を明記する。

- continuation recovery は自動再実行ではない。
- `job_list` 空は「処理未実行」の証明ではない。
- checkpoint は `client_reported`。
- response loss 時は同じ `operation_id` で retryする。
- ambiguous side effect は暗黙にreplayしない。
- `apply_patch` も normal session の root / approval boundary 内で動作する。

COS/Codex由来の実装コードを直接移植する場合は license / notice を確認し、必要なら `THIRD_PARTY_NOTICES*` を更新する。
設計・contractだけを参考に独自実装する場合も、issue/commitに参照元を残す。

---

## 検証

最低限:

```sh
TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT=1 cargo test routed_gateway_contract_matches_checked_in_snapshot
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
(cd gateway && npm test)
git diff --check
```

追加で、一時sessionを使い次をE2E確認する。

```text
start session
  -> long-running job start
  -> checkpoint save(operation_id=A)
  -> response loss相当のexact retry
  -> 新client相当から job_list/work_handoff
  -> existing jobをpoll
  -> apply_patch
  -> read back
```

同じjobの再起動、同じcheckpointの二重create、permission拡張、secret永続化が発生しないことを確認する。

## 完了条件

- TEMOTE-01/02/03 の再開フローが response loss / retry / client切替に対して安全側に倒れる。
- mutating checkpoint operation が exact retry で重複しない。
- ambiguous side effect を勝手に failed/safe-to-retry と分類しない原則がcontract/tests/docsに固定される。
- `apply_patch` が全対象preflight後にのみmutationし、Temoteのsandbox/approval/root boundaryを維持する。
- browser orchestration / worker chats / unrestricted shell / full transcript recorderをTemote coreへ導入していない。
