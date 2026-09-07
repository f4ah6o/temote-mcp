# TEMOTE-06: execution friction → learning → recall loop

- Status: Open
- Date: 2026-09-08 (Asia/Tokyo)
- Priority: P1
- Baseline: `6d8ffd142708285894c9ec92cad2aeccd59fc154` (`main`)
- Related:
  - [TEMOTE-02](20260905-02-scoped-task-checkpoints.md)
  - [TEMOTE-03](20260905-03-read-only-work-handoff.md)
  - [TEMOTE-05](20260908-05-durable-continuation-and-apply-patch.md)
- Reference implementation / design study:
  - https://github.com/Tencent/teamai-cli
  - `src/contribute-check.ts`
  - `src/utils/search-index.ts`
  - `src/code-knowledge-recall.ts`
  - `skills/teamai-share-learnings/SKILL.md`
  - `docs/usage-guide.md`

## 目的

Temote が既に持つ session / sandbox / approval / job / Git の実行観測を利用し、作業中に発生した「摩擦」を bounded な構造化イベントとして記録する。

価値の高い friction が蓄積した session / scoped work から、後続作業で再利用可能な learning candidate を生成できるようにし、次回 session 開始時や明示的な問い合わせ時に関連 knowledge を軽量 recall できる最小ループを作る。

```text
Temote execution
  -> friction events
  -> learning candidate
  -> curated learning
  -> lightweight recall
  -> later session
```

Tencent TeamAI の Team Execution / Team Context / Team Improvement 全体を移植するのではなく、以下の設計原則だけを Temote に合わせて採用する。

1. tool call 数や session 長ではなく friction を learning 候補の主信号にする。
2. Temote が execution layer で直接観測できる事実を優先し、agent hook からの推測を authoritative にしない。
3. issue / checkpoint / handoff と reusable learning を分離する。
4. recall は最初から外部 vector DB や embedding service を必須にしない。
5. knowledge が役に立たなかった事実も knowledge-gap signal として観測する。
6. sandbox / approval / secret non-persistence / audit minimization を一切弱めない。

## なぜ TeamAI をそのまま導入しないか

TeamAI は skills / rules / docs / agents / hooks / MCP definitions / learnings を Git で配布し、Claude Code / Codex / Cursor 等の agent harness を横断管理する control plane として優れている。

一方 Temote の中心責務は local-machine execution の data plane であり、以下を既に ownership している。

- session runtime / lifecycle。
- permitted roots / sandbox。
- local out-of-band approval。
- execute / background jobs。
- filesystem mutations。
- dedicated Git operations。
- host integrations。
- remote ingress と supervisor。

TeamAI 全体を Temote core に取り込むと agent configuration manager / team governance / knowledge manager まで責務が膨らむ。

当面は TeamAI を runtime dependency にせず、friction / learning / recall の設計を Temote-native な小さい surface として実装する。

将来、複数人・複数agentへ skills/rules/MCP 設定を配布する必要が強くなった場合は、その層を TeamAI 等の外部 control plane に任せられる境界を維持する。

## 用語と責務の分離

### Temote session

Temote の session は実 runtime であり、cwd、permitted roots、permission mode、socket、jobs、lifecycle を持つ。

### learning session / friction scope

learning 判定は Temote session 全体に限定しない。
TEMOTE-02 の scoped checkpoint / work identity が実装された場合は、その scope 単位で friction を集約できる設計を優先する。

### issue / checkpoint / handoff

未完了作業、現在状態、再開条件、禁止事項を表す。
これは reusable knowledge ではない。

### learning

問題を解いた後に残す、後続作業へ再利用可能な短い知見。
日記や transcript の保存先にしない。

---

## Phase A: bounded friction event model

### 最初に観測するイベント候補

Temote が execution layer で直接知り得るものを優先する。

```text
approval_requested
approval_denied
sandbox_denied
path_escape_rejected
execute_failed
job_failed
job_cancelled
session_crashed
git_operation_failed
integration_failed
client_operation_retried
ambiguous_mutation_detected
```

以下は Temote 単独では確実に判断できないため authoritative friction として直接記録しない。

```text
user_correction
wrong_reasoning
agent_changed_direction
conversation_interrupt
```

将来 client / agent から明示的な telemetry が渡される場合は `client_reported` として observation source を区別する。

### event schema

第一候補:

```text
FrictionEvent {
  event_id
  session_id
  scope_id?
  occurred_at
  kind
  source
  operation_class?
  tool_name?
  outcome?
  retry_group?
}
```

以下を保存しない。

- command argv 全文。
- stdout / stderr 全文。
- file contents。
- prompt / conversation transcript。
- approval本文。
- authenticated identity の secret-bearing fields。
- env / token / password / credential values。

既存 runtime audit と重複する場合は別ログを無制限に増やさず、共通の bounded event primitive または derived view を検討する。

### retention

- active scope の friction は learning 判定に必要な期間だけ保持する。
- terminal session metadata と同様に bounded retention とする。
- malformed / ambiguous state を cleanup のために勝手に成功・失敗へ正規化しない。
- session ID や scope ID を filesystem path に使う場合は traversal-safe な canonical encoding を使う。

---

## Phase B: friction scoring

TeamAI の考え方を参考にするが、同じ weight をコピーしない。
Temote の failure semantics に合わせて別途 tuning する。

重要原則:

- tool call 数だけでは learning candidate を発火させない。
- repeated failure / denial / crash / ambiguity を主信号にする。
- 1回の expected denial が過大評価されないようにする。
- 同一 root cause から生じる retry storm を無制限加点しない。
- client retry と server-side failure retry を可能なら区別する。

例:

```text
clean 100-tool session
  -> candidate=false

execute failure x1, corrected immediately
  -> low score

same operation fails/retries repeatedly
  -> higher score

approval denied + alternate safe path chosen
  -> candidate likely

ambiguous mutation / session crash during meaningful work
  -> candidate strongly considered
```

score は explanation を機械可読で返せること。
単一 opaque number だけにしない。

```text
{
  score,
  reasons: [
    { kind: "execute_failed", count: 4 },
    { kind: "approval_denied", count: 1 }
  ]
}
```

---

## Phase C: learning candidate

### candidate と authoritative learning を分ける

friction score が threshold を超えても、自動で authoritative knowledge に昇格させない。

```text
friction events
  -> candidate
  -> human/agent review
  -> learning
```

candidate には次のような bounded metadata を持てる。

```text
LearningCandidate {
  candidate_id
  session_id
  scope_id?
  created_at
  friction_summary
  related_checkpoint?
  related_issue_paths?
  status
}
```

candidate 自体に transcript や command output をコピーしない。

### learning format

repo-managed Markdown を第一候補とする。

例:

```markdown
---
title: "Steam CM certificate validation required GnuTLS-enabled Wine"
date: 2026-09-08
tags: [wine, steam, tls, troubleshooting]
domain: technical
---

## Problem
...

## Resolution
...

## Reusable lesson
...

## Verification
...
```

必須要件:

- title。
- date。
- bounded tags。
- concise problem / resolution / reusable lesson。
- verification level または evidence class。
- secret / transcript / raw logs の貼付禁止。

`issues/open` は未完了 work item のまま維持し、learning store と兼用しない。

理想的な lifecycle:

```text
issues/open
  -> work completed
  -> issues/resolved or checkpoint terminal state
  -> learning candidate
  -> curated learning
```

---

## Phase D: lightweight recall

### 初期実装

外部 embedding API / vector DB を必須にせず、ローカル deterministic index から始める。

第一候補:

- title token match。
- tags match。
- body excerpt token match。
- simple IDF-like weighting。
- knowledge type / domain weighting。
- optional explicit relevance feedback。

index は再構築可能な derived artifact とする。
authoritative knowledge は Markdown 側。

### recall result

結果は explainable にする。

```text
RecallHit {
  learning_id
  title
  score
  matched_terms
  missing_terms
  tags
  verification
}
```

query と hit の理由を確認できること。

### automatic recall

初期段階では、すべての tool call 前に検索しない。

候補:

1. session / scoped work 開始時の明示 recall。
2. handoff / checkpoint resume 時の recall。
3. client が `recall` tool を明示呼び出し。

自動 injection は false positive / context pollution を測定してから検討する。

---

## Phase E: knowledge-gap signal

recall を実行したが useful hit が無かった場合、それ自体を improvement signal として扱えるようにする。

例:

```text
recall(query)
  -> no relevant hit
  -> significant friction occurs
  -> task succeeds
  -> candidate score bonus
```

ただし「検索hit無し」だけで learning candidate を作らない。
実際の friction または明示的な contribution intent と組み合わせる。

将来 relevance feedback を導入する場合も、user-specific preference と reusable technical knowledge を混同しない。

---

## Phase F: MCP / CLI surface

実装時に exact naming は再検討するが、概念surface候補:

```text
friction_summary(session_id, scope_id?)
learning_candidate_list(session_id?, scope_id?)
learning_candidate_show(candidate_id)
recall(query, scope?)
```

learning の確定書き込みは、初期実装では generic `write_file` や Git flow を利用してもよい。
専用 `learning_publish` を作る場合は mutation / approval / idempotency contract を先に定義する。

remote MCP から勝手に team-wide knowledge を publish する設計にはしない。

CLI では診断用に:

```text
temote-mcp friction ...
temote-mcp recall ...
```

のような owner-side surface を検討できる。

---

## Security / privacy invariants

この機能追加によって Temote の安全境界を広げない。

必須:

- normal session の sandbox / network restriction を維持。
- permitted roots を維持。
- remote session が yolo を取得できない契約を維持。
- approval denial を迂回しない。
- OpenAI / client safety block の迂回を目的としない。
- secret-bearing event payload を永続化しない。
- conversation transcript を収集しない。
- command output を knowledge extraction のために自動保存しない。
- recall index は permitted knowledge roots 以外を勝手にcrawlしない。
- knowledge publish は既存 Git / approval trust model を尊重する。

OpenAI/client側で tool call が block されたことを Temote が直接観測できない場合、Temote event として捏造しない。
client が明示的に報告できる将来contractを作る場合は `source=client_reported` とする。

---

## 非目標

初期実装では以下を行わない。

- TeamAI CLI 自体への runtime dependency。
- TeamAI の `teamai pull/push` の再実装。
- Claude / Codex / Cursor の設定ディレクトリを Temote が横断管理すること。
- roles / tags subscription / team membership management。
- MCP server definitions の agent config への自動inject。
- dashboard / usage analytics product。
- full semantic vector search。
- LLM による全session transcript の自動要約。
- codebase knowledge graph の Temote core への内蔵。
- friction score による自動 side effect 実行。

codebase graph が必要になった場合は、Temote core とは別の indexer / plugin / external control-plane component として評価する。

---

## テスト

最低限以下を deterministic に検証する。

### event / privacy

- `friction_event_records_operation_class_without_command_argv`
- `friction_event_never_persists_stdout_or_stderr`
- `friction_event_never_persists_secret_env_values`
- `friction_event_distinguishes_observed_from_client_reported`
- `friction_retention_is_bounded`

### scoring

- `clean_high_tool_count_session_does_not_trigger_candidate`
- `repeated_same_root_failure_is_bounded`
- `approval_denial_contributes_without_recording_approval_body`
- `ambiguous_mutation_is_high_value_friction`
- `score_exposes_machine_readable_reasons`

### candidate

- `candidate_does_not_copy_transcript`
- `candidate_does_not_become_learning_automatically`
- `candidate_links_checkpoint_without_embedding_checkpoint_text`

### recall

- `recall_index_is_rebuildable_from_authoritative_markdown`
- `recall_reports_matched_and_missing_terms`
- `recall_respects_configured_knowledge_root`
- `recall_does_not_require_network`
- `recall_no_hit_alone_does_not_create_candidate`

### safety

- `learning_feature_does_not_widen_permitted_roots`
- `learning_feature_does_not_bypass_approval`
- `remote_learning_surface_cannot_enable_yolo`

---

## 実装順序

1. friction event schema と privacy/retention invariants を決める。
2. 既存 runtime audit との重複を整理する。
3. execution-layer event emission を最小種類で実装する。
4. explainable friction summary / score を追加する。
5. learning candidate を追加する。
6. repo-managed learning format を定義する。
7. deterministic lightweight recall index を追加する。
8. checkpoint / handoff resume と recall を接続する。
9. knowledge-gap signal を追加する。
10. 実運用データを見て threshold / automation を調整する。

## 完了条件

- Temote が主要な execution friction を secret-free / bounded event として記録できる。
- 長いだけの正常sessionを learning candidate にしない。
- friction candidate と authoritative learning が分離されている。
- reusable learning を repo-managed text として残せる。
- network不要の lightweight recall ができ、match理由を説明できる。
- handoff / resume 時に関連learningを検索できる。
- knowledge gap を future candidate scoring に利用できる。
- 既存 sandbox / approval / permission / secret semantics に回帰がない。
- TeamAI や特定agent harnessを runtime dependency にしない。

## 将来の境界

Temote は引き続き execution/runtime を ownership する。

```text
Agent / team control plane
  skills / rules / learnings / broader knowledge
                |
               MCP
                v
Temote execution plane
  supervisor / session / sandbox / approval / jobs / git
                |
                v
             host OS
```

将来 TeamAI 等を併用する場合も、Temote の friction/recall primitive は protocol/API boundary を通して利用でき、agent config management を Temote core に取り込まなくてよい構造を維持する。
