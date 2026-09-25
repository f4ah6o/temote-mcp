# O3: observation worker substrate and friction feedback loop

Status: proposed implementation packets
Repository: `f4ah6o/temote-mcp`
Parent: `issues/open/20260925-observation-context-memory-plane.md`
Depends on: O1 observation journal + O2 deterministic context resolver (landed in PR #60)
Priority: high
Created: 2026-09-26 (Asia/Tokyo)

## 1. Goal

Temote を使って実作業を行う coding agent に、memory maintenance や Temote 自身の friction reporting を背負わせない。

O1/O2 で自動記録された observation を、interactive coding task とは独立した worker が後追い処理し、

- reusable knowledge
- Temote 固有の friction candidate
- `issues/open/` への改善記録
- focused pull request

へ変換できるようにする。

coding task の成功/失敗は worker/publisher の成功/失敗から独立させる。

## 2. Architecture

```text
coding agent / caller
        |
        v
Temote orchestration boundary
        |
        v
Observation Journal (O1)
        |
        v
Observation Worker Runtime
        |
        +----------------------+
        |                      |
        v                      v
Memory Extractor        Friction Observer
        |                      |
        v                      v
Knowledge Store         FrictionCandidate Store
        |                      |
        v                      v
Context Resolver        Publisher Agent
                               |
                               v
                    issues/open/*.md + PR
```

worker runtime は共通だが consumer の責務と権限は分離する。特に publisher だけが `f4ah6o/temote-mcp` への Git/GitHub write capability を持ちうる。

## 3. O3a — Observation worker substrate

### Responsibilities

- session ごとの observation revision checkpoint を保持する。
- bounded batch で observation を読む。
- same-batch retry を idempotent にする。
- consumer ごとの output dedupe key を保持する。
- best-effort wakeup と recovery scan を両立する。
- stale/degraded/last-processed revision を operator が確認できる。
- consumer failure を coding task state に伝播させない。

### Cursor model

global revision を新設しない。authority は少なくとも次とする。

```text
session_id -> last_processed_revision
```

append notification は最適化であり correctness の前提にしない。通知が落ちても bounded periodic scan で未処理 session を再発見できること。

### Acceptance

- worker restart 後も checkpoint から再開できる。
- 同一 batch の再実行で duplicate output が増殖しない。
- 1 session の corrupt/degraded journal が他 session の処理を止めない。
- worker が停止していても task execution は通常どおり進む。

## 4. O3b — Memory extractor

既存 O3 Memory Worker の knowledge extraction 責務をこの consumer に移す。

- episode grouping
- fact / decision / constraint / unresolved extraction
- support observation refs required
- dedupe / supersession
- unsupported assertion を current fact にしない
- worker/model を交換して再生成可能

この packet の詳細 authority は親 issue の knowledge model に従う。

## 5. O3c — Friction observer

### Principle

raw observation 1件をそのまま「不満」に変換しない。まず deterministic signal で候補 episode を抽出し、その bounded episode と support refs だけを evaluator に渡す。

### Initial deterministic signals

- 同一 task/operation に対する repeated retry
- `reconciliation_required` / `unknown` / recoverable error の反復
- approval / waiting_input の不要な往復
- session restart / reconnect / recovery churn
- 同一目的に対する過剰な status/get/control probe
- backend 固有 workaround の反復
- terminal success までに materially excessive な回復操作が必要だったケース

signal は friction の結論ではない。evaluator の候補生成トリガーに限定する。

### Classification

evaluator は少なくとも以下を区別する。

- `temote_friction`
- `target_repository_bug`
- `upstream_transient`
- `insufficient_evidence`
- `known_existing_issue`

`target_repository_bug` / `upstream_transient` / `insufficient_evidence` は Temote issue の自動候補に昇格させない。

### FrictionCandidate

Conceptual fields:

```text
id
fingerprint
status
scope
summary
impact
expected_lower_friction_behavior
generic_resolution_hypothesis
acceptance_criteria[]
support_observation_refs[]
support_evidence_refs[]
related_sessions[]
confidence
producer
produced_at
```

candidate は source of truth ではない。support refs から再評価可能であること。

同じ fingerprint が複数 session/repository で再発した場合は recurrence signal として扱えるが、repository A の private context を repository B の candidate body に混ぜない。

### Acceptance

- coding agent の prompt に friction-reporting instruction を追加しなくても candidate を作れる。
- candidate は必ず support refs と acceptance criteria を持つ。
- target repository bug を Temote issue として誤って publish しないための classification gate がある。
- observer failure は coding task state を変更しない。

## 6. O3d — Friction publisher

publisher は observer と分離した write-capable consumer とする。

### Flow

1. supported `FrictionCandidate` を読む。
2. `f4ah6o/temote-mcp/issues/open/` の既存 entry を semantic/fingerprint で検索する。
3. 重複なら再発 evidence と support refs を既存 entry に追記する。
4. 新規なら dated Markdown を作る。
5. issue Markdown には observed event / evidence / impact / expected behavior / generic resolution / acceptance criteria を含める。
6. focused PR を作る。
7. auto-merge しない。

### Permission boundary

- observation worker runtime 自体には Git/GitHub write を必須にしない。
- publisher の write scope は `f4ah6o/temote-mcp` に限定可能であること。
- target project の filesystem roots を拡張して Temote issue を書かない。
- publisher が使う workspace/session は Temote repo 専用にする。

### Failure semantics

- GitHub write path unavailable
- duplicate detection failure
- PR creation failure
- reviewer reject

のいずれでも coding task と observation checkpoint を failed にしない。candidate は retained/retryable に残す。

### Acceptance

- 同じ friction で duplicate issue/PR を増殖させない。
- publish failure 後に同じ candidate を安全に再試行できる。
- PR が作成されても自動 merge されない。
- issue/PR body から support refs へ追跡できる。

## 7. Agent Skill behavior

`skills/temote-mcp/SKILL.md` では coding agent に次を要求しない。

- 別 project の作業中に `issues/open/` を作る。
- Temote repo へ session/workspace を切り替える。
- filesystem root を広げる。
- friction report 用 PR を作る。

coding agent は通常どおり exact error/state を保持し、uncertain side effect を reconcile し、依頼された作業を完了する。friction の後処理は observer/publisher の責務とする。

observer/publisher 未実装期間は、自動 feedback を unavailable と扱う。旧運用へ暗黙 fallback しない。ユーザーが明示的に Temote friction report を依頼した場合、または Temote 自身を開発している task の場合だけ通常の issue/PR 作業を行う。

## 8. Public surface

最初から public MCP tools を増やす必要はない。

owner/operator-only candidate surface の候補:

```text
temote-mcp worker status
temote-mcp worker run
temote-mcp friction candidate list
temote-mcp friction candidate inspect
temote-mcp friction publisher run
```

raw observation body と private candidate content を通常の remote MCP surface に出さない。

## 9. Non-goals

- worker による coding task の steer/resume/interrupt
- autonomous bug fixing
- generic planner/workflow engine
- target repository の issue 自動作成
- friction PR の auto-merge
- hidden chain-of-thought capture
- cross-repository private context aggregation

## 10. Implementation order

```text
O1 observation journal        [done]
        |
O2 deterministic context      [done]
        |
O3a worker substrate
       / \
      v   v
 O3b memory  O3c friction observer
                 |
                 v
             O3d publisher

O3b -> O4 knowledge-aware resolver
```

O3a を先に実装し、memory と friction のために別々の scheduler/checkpoint 実装を作らない。

## 11. Completion criteria

- [ ] O3a worker substrate が session revision cursor を安全に追跡する。
- [ ] O3b memory extractor が support refs 付き knowledge を生成する。
- [ ] O3c friction observer が coding agent の追加作業なしで candidate を生成する。
- [ ] O3c が Temote friction と target/upstream 問題を区別する。
- [ ] O3d が existing issue を dedupe して new-or-update PR を作る。
- [ ] publisher failure が coding task / worker checkpoint に影響しない。
- [ ] publisher が auto-merge しない。
- [ ] Skill が coding agent 自身への routine friction reporting を要求しない。
