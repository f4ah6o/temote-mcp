# Temote self-improvement dogfood protocol: scenario + observation + evaluator

Status: done
Model: unknown
Created: 2026-09-28
Updated: 2026-09-28
Branch: feat/20260928-self-improvement-dogfood
Scope: repository-owned self-improvement/dogfood protocol and first live cycle
Related:

- `skills/temote-mcp/SKILL.md` (current operating guidance and friction feedback)
- `issues/open/20260924-temote-development-harness-restructure.md` (umbrella development-harness architecture)
- `issues/open/20260925-observation-context-memory-plane.md`
- `issues/done/20260926-cloud-observation-knowledge-plane.md`
- `issues/open/20260926-temote-fabric-product-boundary.md`
- `issues/open/20260927-bounded-wait-for-delegated-tasks.md`

## Goal

Temote 自身を実際の orchestration control plane として dogfood し、その利用中に発生した摩擦を一次データとして記録し、修正後に**同じ logical scenario を再実行して before / after を比較できる自己改善ループ**を作る。

これは「repository を読んで LLM が使いにくそうな点を想像する」仕組みではない。中心となる evidence は、実際の client / agent が Temote を使って観測した以下である。

- logical operation ごとの tool-call 数
- 明示入力した引数
- opaque ID の手作業引き回し
- polling / rediscovery / reconnect の追加手順
- unchanged-state response
- structured error / retryability / suggested next action
- duplicate side-effect risk
- terminal state の判定可能性
- response size / bounded evidence
- recovery に必要だった追加 call

改善後は同一 scenario を candidate に対して再実行し、架空の数値や印象評価ではなく、保存された observation から比較する。

## Why a Skill alone is insufficient

現在の `skills/temote-mcp/SKILL.md` は、Temote 固有 friction を product feedback として扱う原則をすでに持つ。一方、Skill は agent の判断・手順を記述するものであり、次を再現可能に測る test harness ではない。

- logical operation と raw MCP/tool call の対応
- baseline / candidate の固定
- event-level observation
- tool-call count
- retry / reconnect cost
- before / after の機械比較
- regression acceptance

したがって self-improvement を巨大な Skill prompt として実装しない。

## Architecture

責務を次の 6 層に分ける。

```text
Agent Skill
  WHY / WHEN / decision policy

Scenario
  WHAT to exercise

Local dogfood harness
  HOW to execute + measure

Observation log
  WHAT actually happened

Evaluator
  DID it improve

Repository tests / release policy
  DID behavior remain correct / IS it publishable
```

概念フロー:

```text
                 repository-owned scenario
                           |
             +-------------+-------------+
             |                           |
        Agent Skill                local harness
   judgment/orchestration       execution/measurement
             |                           |
             +-------------+-------------+
                           |
                    observations
                           |
                    friction inventory
                           |
                 implementation change
                           |
                  same scenario rerun
                           |
                   before / after
                           |
                     evaluator
```

## Non-goals

初期実装では以下をしない。

- LLM の自由文「使いにくかった」を primary metric にする
- tool-call 数だけを単一 score にして最適化する
- one-shot の巨大万能 MCP tool を goal とする
- scenario に現在の具体 MCP tool sequence を固定する
- model/provider を scenario に hard-code する
- self-improvement run ごとに必ず release する
- Temote の safety invariant、idempotency、bounded evidence を call 数削減のために弱める
- hidden chain-of-thought を observation として保存する

## Core concept: logical operation

Scenario の単位は MCP tool call ではなく **logical operation** とする。

例:

```text
discover_session
select_repository
inspect_session
start_implementation_agent
rediscover_task
wait_until_terminal
read_terminal_result
inspect_diff
run_tests
inspect_git_status
```

baseline では `discover_session` に 3 calls 必要で、candidate では 1 call になる可能性がある。Scenario が `host_list -> session_list -> session_info` のように concrete tool sequence を固定すると、API 改善そのものを regression と誤認する。

Scenario は目的と assertion を記述し、adapter / executor が現在の tool surface から実行方法を解決する。

## Scenario format

最初は YAML または JSON の repository-owned declarative format を想定する。format は実装 packet で固定する。

例:

```yaml
name: delegation-lifecycle

goal:
  start_two_agents_and_recover_their_results

steps:
  - discover_session
  - select_repository
  - inspect_session
  - start_agent:
      backend: devin
      role: implementation
  - start_agent:
      backend: opencode
      role: implementation
  - rediscover_tasks
  - wait_until_terminal
  - inspect_results
  - inspect_diff
  - run_tests
  - inspect_git_status

assertions:
  - no_duplicate_task
  - terminal_state_is_unambiguous
  - tasks_can_be_rediscovered
  - retry_is_machine_decidable
```

### Scenario invariants

- logical operation は stable semantic name を持つ。
- tool names は scenario contract にしない。
- scenario は server implementation details を知らない。
- safety / compatibility assertions は call-count reduction より優先する。
- live provider unavailable は deterministic failure と区別し、`blocked/not_run` を明示する。
- baseline と candidate は同じ scenario revision で比較する。

## Initial scenario suite

最初は巨大な release E2E 一本ではなく、次の小さい scenario を composable にする。

### S1 delegation-happy-path

```text
session discovery
-> repository/root selection
-> session inspection
-> agent start
-> running observation
-> terminal result
-> diff/tests/status
```

Acceptance:

- terminal state が一意に判断できる
- terminal result を bounded interface で取得できる
- required job/task を running のまま残さない

### S2 task-rediscovery

途中で task ID を caller state から意図的に捨てる。

```text
start
-> lose local task id
-> reconnect / rediscover
-> recover task identity
-> continue to terminal
```

Acceptance:

- duplicate start をしない
- session / task discovery だけで既存 task を再追跡できる
- unavailable backend と empty result を混同しない

### S3 transient-poll-failure

poll / status retrieval に transient failure を注入または fixture 化する。

```text
running
-> transient poll failure
-> existing task lookup
-> session state check
-> safe resume
-> terminal
```

Acceptance:

- transient error 単独で failed 扱いしない
- unsafe replay をしない
- retryability が machine-readable

### S4 duplicate-start

uncertain start response を再現する。

Acceptance:

- same operation/idempotency key で exact retry できる
- new operation ID による duplicate side effect を必要としない
- accepted / reconciliation-required / retryable の区別が明示される

### S5 baseline-candidate-self-host

Temote が Temote 自身を変更する場合の control-plane 分離を検証する。

Acceptance:

- baseline repository HEAD と running baseline server build identity を別々に記録する
- repository が candidate HEAD に進んでも baseline identity は不変
- candidate は別 process/session/port 等で識別可能
- baseline / candidate の observation が混ざらない
- baseline を止める場合は running required jobs/tasks がないことを先に確認する

### S6 release-qualification composition

S1-S5 と repository checks をまとめる上位 scenario。release は self-improvement core の必須 side effect にしない。

```text
observe
-> improve
-> qualify
-> optional release policy
```

## Observation protocol

各 tool interaction について、巨大 transcript ではなく比較に必要な bounded event を残す。

概念 schema:

```text
run_id
scenario_id
scenario_revision
phase                 baseline | candidate

logical_operation
call_index
tool

request:
  explicit_arguments
  repeated_arguments
  opaque_ids

response:
  state
  structured_error
  retryable
  suggested_next_action
  output_bytes

decision:
  next_action_decidable
  missing_information

recovery:
  attempted
  extra_calls

duration
refs
```

### Derived metrics

raw observation から少なくとも以下を機械計算する。

```text
tool_calls_per_operation
explicit_argument_count
repeated_argument_count
opaque_id_handoffs
poll_count
unchanged_poll_count
rediscovery_calls
recovery_calls
response_bytes
ambiguous_terminal_states
duplicate_start_attempts
```

LLM による qualitative note は追加可能だが、primary evidence にはしない。

## Baseline / candidate identity

一回の run は immutable snapshot identity を持つ。

```text
ImprovementRun
├─ baseline_snapshot
│  ├─ repository_head
│  ├─ server_build_identity
│  ├─ server_contract_fingerprint
│  └─ environment_capabilities
├─ baseline_scenarios
│  └─ observations
├─ friction_inventory
├─ changes
├─ candidate_snapshot
├─ candidate_scenarios
│  └─ observations
└─ comparison
```

`repository_head` と `running server build identity` は同一視しない。

Temote self-hosting 中は、変更後 repository HEAD と稼働中 baseline binary/server が異なる状態を正常な中間状態として扱う。

## Friction inventory

baseline run 後、observation refs を根拠に structured inventory を作る。

```text
friction_id
scenario
logical_operation

reproduction:
  event_refs[]

baseline:
  calls
  arguments
  bookkeeping
  recovery_steps

desired:
  calls
  arguments
  recovery_steps

severity:
  P0 | P1 | P2

reason
server_side_candidate
compatibility_risk
implementation_owner
```

「一般的に便利そう」だけでは high priority にしない。

## Severity

可能な範囲で機械的な candidate classification を行い、reviewer が確定する。

### P0 candidate

実測で次のいずれかが発生した場合:

- work cannot continue
- task/job is lost or cannot be rediscovered
- terminal state cannot be determined
- safe retry cannot be determined
- duplicate side effect is observed or practically unavoidable
- reconnect/resume cannot recover the operation

### P1 candidate

実測で次が繰り返された場合:

- same opaque ID / parameter manually propagated multiple times
- status/result retrieval requires avoidable extra calls
- unchanged polling produces substantial overhead
- server-known root/cwd/backend/model state must be repeatedly re-entered
- reconnect/rediscovery has avoidable bookkeeping

Threshold は implementation packet で fixture とともに固定する。

### P2 candidate

実利用中に:

- schema/name/enum/default が不明瞭
- error が structured でない
- retryability が不明
- next action が response だけから判断できない
- blocking/non-blocking semantics が不明

## Comparison

単一の friction score は作らない。

例:

```text
metric                  baseline   candidate
tool calls                    7           3
manual opaque IDs             4           1
recovery calls                3           1
response bytes              18K          5K
retry machine-decidable      no          yes
safety regression             -           no
```

比較軸:

- calls
- explicit args
- opaque bookkeeping
- recovery steps
- poll overhead
- response bytes
- decision ambiguity
- safety invariants
- compatibility
- observability
- idempotency

Candidate qualification は「少ない call 数」ではなく、対象 friction の acceptance criteria を満たし、safety/compatibility regression がないことを条件にする。

## Skill responsibility

既存 `temote-mcp` Skill は Temote の安全な操作 guidance として維持する。

self-improvement 用 guidance を追加する場合も、scenario/test logic を Skill 本文へ複製しない。

概念 responsibility:

```text
1. baseline identity を固定する
2. relevant scenario suite を選ぶ
3. dogfood harness を実行する
4. observation を読む
5. friction inventory を作る
6. implementation task に分解する
7. implementation agent に委譲する
8. independent reviewer に review させる
9. candidate で同じ scenario を再実行する
10. before / after を評価する
11. repository/release policy に従って qualification する
```

候補:

- generic `skills/self-improve/`
- Temote-specific `skills/temote-dogfood/`

初期実装では Temote repository-owned scenarios を先に確立し、Skill 名/packaging は別 packet で決める。

## Local dogfood harness responsibility

普通の unit test だけに live orchestration を押し込まない。

概念 layout:

```text
dogfood/
  scenarios/
    delegation-lifecycle.*
    task-rediscovery.*
    transient-poll-failure.*
    duplicate-start.*
    baseline-candidate-self-host.*
  schemas/
  fixtures/
  runs/              # ignored/local artifacts
```

local harness は:

- scenario execution
- event capture
- bounded artifact storage
- metrics calculation
- baseline/candidate comparison
- fault injection / fake backend support
- live acceptance adapter

を担当する。

CLI UX は implementation packet で決める。例えば将来:

```text
temote-mcp selftest baseline
temote-mcp selftest candidate
temote-mcp selftest compare <baseline-run> <candidate-run>
```

としてもよいが、本 issue では CLI surface を固定しない。

## Deterministic vs live gates

### Deterministic CI

CI に置けるもの:

- scenario schema validation
- observation schema validation
- logical-operation executor state machine
- fake backend
- retry/idempotency behavior
- transient failure fixtures
- duplicate-start fixture
- metric calculation
- before/after evaluator
- report serialization

### Host acceptance

real local environment が必要:

- real Temote process
- process restart/reconnect
- session rediscovery
- baseline/candidate self-host
- local control-plane identity
- dirty worktree safety

### Provider acceptance

credential/provider dependent:

- real Codex
- real OpenCode
- real Devin ACP / Devin Cloud
- actual model/provider propagation
- live long-running delegation

Provider acceptance が unavailable の場合、deterministic CI PASS に読み替えない。

## Agent role profiles

Scenario は具体 model 名を持たず、role を指定する。

```yaml
agent:
  role: implementation
  backend: devin
```

role profile が current model/provider/tier を解決する。

現在の運用例:

```yaml
profiles:
  implementation:
    codex:
      model: gpt-6-luna
      effort: max
    opencode:
      model: opencode-go/deepseek-v4.1-flash
    devin:
      model: swe-2
      effort: max
      tier: promo

  review:
    codex:
      model_class: gpt-6-astra-or-sol
      effort: high-or-xhigh
    opencode:
      model_class: openai-gpt-6-astra-or-sol
      effort: high-or-xhigh
    devin:
      model: opus-5.5
```

Model availability/tier は time-dependent なので scenario contract にしない。profile は explicit run metadata として保存する。

## Observer / Implementer / Evaluator separation

自己改善 agent が自分自身の成功を自由文で確定しない。

役割を少なくとも次に分ける。

```text
Observer
  facts/events only

Implementer
  change the product based on accepted friction

Evaluator
  compare baseline/candidate against assertions
```

可能なら Evaluator は implementation rationale より先に observation/comparison を評価できるようにする。

## Working-tree safety

Dogfood run は既存の repository safety policy を継承する。

- initial repository state を snapshot に含める
- unrelated dirty changes を run-owned change と混同しない
- reset/clean/stash/checkout による他作業者変更の破棄をしない
- duplicate agent execution を避ける
- candidate self-host のために baseline control plane を不用意に停止しない
- baseline と candidate の VCS/workspace identity を明示する

Fabric / jj / VCS transaction の既存 issue と競合する実装をこの harness に持たせない。harness は VCS orchestration policy の consumer であり、別の VCS proxy を発明しない。

## Self-improvement report

run の最終 artifact は自由文だけでなく structured report を持つ。

```text
SelfImprovementReport

baseline
scenario_results

frictions[]
changes[]

candidate
comparison[]

regressions[]
tests[]

qualification:
  qualified | blocked

followups[]
```

`qualified` は release 済みという意味ではない。

意味は:

> candidate が対象 friction の acceptance criteria を満たし、要求された regression gates を悪化させていない。

## Release policy

Release は core self-improvement loop から分離する。

```text
observe
improve
qualify
release
```

通常の dogfood run は `qualify` までで終了可能。

release qualification scenario が要求された場合のみ、repository の既存 CalVer workflow を使い、独自 release mechanism は追加しない。

## Proposed implementation packets

この umbrella/design issue を一度に巨大実装しない。

### D1 scenario + observation schemas

- scenario logical-operation schema
- immutable run metadata
- observation event schema
- bounded serialization
- schema tests

Acceptance:

- example scenarios validate
- invalid/missing operation identity fails deterministically
- baseline/candidate identity cannot be silently conflated

### D2 deterministic harness core

- scenario runner state machine
- fake backend/tool adapter
- event recorder
- fault injection
- metric derivation

Acceptance:

- happy path
- transient poll failure
- uncertain start
- rediscovery
- duplicate prevention

を external provider なしで再現できる。

### D3 comparator + friction inventory

- before/after vector comparison
- P0/P1/P2 candidate classification
- observation refs
- SelfImprovementReport

Acceptance:

- invented metrics cannot appear without source events
- `not_run` is distinct from PASS
- safety/compatibility regression blocks qualification

### D4 Temote live adapter

current public MCP/task surface に対して scenario logical operation を実行する adapter。

Acceptance:

- baseline live run を保存できる
- task rediscovery を実測できる
- running -> terminal を追跡できる
- transient connection failure で duplicate task を開始しない

### D5 baseline/candidate self-host acceptance

- repository/server identity split
- candidate launch isolation
- rerun same scenario revision
- comparison artifact

Acceptance:

- baseline observation と candidate observation が別 run identity を持つ
- repository HEAD change だけで running server identity を上書きしない

### D6 Skill integration

- self-improvement guidance
- scenario selection
- observer/implementer/evaluator separation
- report consumption

Acceptance:

- Skill に scenario implementation details を複製しない
- local harness unavailable 時に架空の実測値を生成しない

### D7 release qualification composition

既存 repository checks / CalVer workflow との integration。

Acceptance:

- ordinary dogfood run は release side effect を持たない
- release mode では final diff/tests/status/CI/action result を distinct state として記録する

## Acceptance criteria for this design

実装後、少なくとも次を実証できること。

1. 同一 logical scenario を baseline と candidate で再実行できる。
2. tool-call count は observation から自動算出される。
3. required explicit arguments と opaque-ID bookkeeping を比較できる。
4. task ID を失っても rediscovery scenario で復旧を実証できる。
5. transient poll failure が duplicate task start を生まない。
6. uncertain start の retryability/idempotency を fixture と live acceptance の両方で評価できる。
7. terminal state ambiguity を evaluator が検出できる。
8. baseline repository HEAD と baseline running server build identity を別々に保持できる。
9. candidate を baseline と混同せず dogfood できる。
10. call count が減っても safety/compatibility regression があれば qualified にならない。
11. provider unavailable / host-blocked / not-run を PASS と扱わない。
12. model/provider/tier の変更は role profile の変更で吸収でき、scenario を書き換えない。
13. Skill は decision/orchestration policy に留まり、measurement truth は local harness/observations が持つ。
14. report に記載する before/after 数値には必ず source observation refs がある。
15. release は self-improvement core から分離され、明示された release-qualification run のみ既存 CalVer workflow を利用する。

## Completion condition

この issue 自体の完了条件は、D1-D7 の child packets が実装・検証され、少なくとも Temote の実 live dogfood 1 cycle について次の artifact が保存・レビューされること。

- baseline snapshot
- baseline scenario observations
- friction inventory
- implemented change refs
- candidate snapshot
- candidate scenario observations
- before/after comparison
- regression/test results
- qualification result
- follow-up friction

その時点で「Temote が自己改善できる」とは、agent が自称することではなく、**同じ repository-owned experiment を baseline/candidate に適用し、観測可能な friction reduction と invariant preservation を再現できること**を意味する。

## Implementation record

2026-09-28: D1-D7 の repository-owned harness、fixture、live MCP adapter、比較器、Skill guidance、release qualification gate を実装した。実 live cycle の baseline/candidate snapshot、bounded observation、friction inventory、変更参照、before/after、回帰チェック、判定、follow-up は [評価記録](../../docs/evaluations/self-improvement-20260928/README.md) に保存した。対象は端末結果の evidence 参照を再利用する client workflow で、`read_terminal_result` は 2 call から 1 call に減った。candidate assertion とローカル回帰ゲートは PASS、評価器はこの対象変更を `qualified` と判定した。リリースは実行していない。

Live task rediscovery、transient poll fault、duplicate start、self-host identity の各 artifact も同評価記録にある。CI/action の結果がない release-qualification scenario は `blocked` と記録した。別 server build の同時運用、他 provider の live acceptance、cross-process terminal evidence handoff、bounded wait は残存 follow-up であり、この client workflow の改善を超えて完了したとは扱わない。
