# `local_agent_run` で Codex reasoning effort を直接指定できない

Status: doing / implementation landed; docs + rebuilt-runtime canary remain
Model: gpt-5.6-sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 developer workflow friction
Type: local agent / execution contract

## Observed

現行 `local_agent_run` の public schema は次だけを受け取る。

```text
session_id, agent, task, cwd?, access, model?, profile?
```

`effort` / `reasoning_effort` は存在しない。

Codex adapter は `model` を `--model`、`profile` を `--profile` に渡すが、reasoning effort の config override は構築していない。一方、既存の Codex delegation backend は `model_reasoning_effort=<value>` を明示的に渡す実装を既に持つ。

さらに `local_agent_run` は invocation ごとに private `CODEX_HOME` を作成し、元の Codex state から import するのは `auth.json` だけである。installed Codex 0.147.0 の `codex exec --help` では `--profile <CONFIG_PROFILE_V2>` は `$CODEX_HOME/<name>.config.toml` を layer する仕様だが、その profile file は private `CODEX_HOME` へ import されない。

したがって、通常の `~/.codex/xhigh.config.toml` 等を用意して `profile="xhigh"` を渡すことは、現行 broker では supported な effort 指定手段にならない。

## Problem

実装 agent を `gpt-5.6-luna` / `max`、レビューを `xhigh` 等で明示的に使い分けたい場合、caller が `local_agent_run` だけでは reasoning effort を固定できない。model は指定できるのに effort は ambient/default に依存するため、再現性・評価・usage 比較・handoff の契約が不完全になる。

`profile` に effort の意味を持たせる回避策は次の理由で不適切。

- Codex と OpenCode で `profile` の意味が異なる (`--profile` vs `--agent`)。
- private agent state に user profile file を import していない。
- profile 全体を import すると、effort 以外の user config / provider / sandbox / tool policy を意図せず持ち込む可能性がある。

## Existing alternative surface

current connected MCP surface の `codex_task_start` は `model` と `effort` を必須入力として受け付ける。したがって Codex app-server retained task では `max` / `xhigh` 等を明示する経路が既にある。

ただしこれは `local_agent_run` と同じ契約ではない。

- `codex_task_start`: workspace-write の retained app-server thread/turn、operation_id 必須、後続 get/control を持つ。
- `local_agent_run`: Codex/OpenCode 共通の one-shot broker、`read_only | workspace_write` を選択可能。

よって `codex_task_start` の存在は、read-only one-shot review や OpenCode/Codex 共通 orchestration で `local_agent_run` に effort がない問題を解消しない。両 surface の使い分けを docs で明示する。

## Proposed contract

`local_agent_run` に optional `effort` を追加する。

```text
local_agent_run({
  session_id,
  agent: "codex",
  task,
  access,
  model?,
  effort?,
  profile?
})
```

Codex の場合のみ、broker-owned command に安全に quote した exact config override を追加する。

```text
--config model_reasoning_effort="max"
```

OpenCode に `effort` が渡された場合は fail-closed にする。OpenCode 固有の model variant/agent semantics が必要なら別 contract として扱い、Codex reasoning effort と混同しない。

## Constraints

- raw Codex argv / arbitrary `--config` を caller に公開しない。
- effort から config-key injection できないこと。
- `--ignore-user-config`, private `CODEX_HOME`, imported auth isolation, permission profile, workspace containment を維持する。
- `profile` の既存 API を破壊しない。
- requested effort と observed effort を混同しない。observed 値が event から取得できない場合は null/unknown のままにする。

## Acceptance criteria

- [x] `local_agent_run(agent="codex", effort="max")` が bounded public schema で受理される実装を追加。
- [x] `xhigh`, `max`, `high` 等の effort が safe TOML basic string として `model_reasoning_effort` override に渡る。
- [x] OpenCode + `effort` は executable resolution 前に明確な validation error になる。
- [x] malformed / oversized effort は command construction 前に fail-closed。
- [x] raw argv / arbitrary config key は公開されない。
- [ ] existing model/profile/auth/sandbox regression tests を full deterministic gate まで確認する。
- [ ] docs/usage(.ja).md が profile と effort の意味を区別して説明する。

## Partial implementation / verification (2026-09-16)

切り上げ時点で core implementation は以下まで完了した。

- `src/local_agent.rs`
  - optional `effort` inputを追加。
  - bounded simple-name validation (`MAX_EFFORT_BYTES=128`)。
  - Codexのみ受理し、OpenCodeでは fail-closed。
  - Codex argvには broker-owned `--config model_reasoning_effort="<quoted>"` を追加。
  - `profile` は従来どおり別入力のままで、effortとの意味を混同しない。
- `src/mcp.rs` / `gateway/src/protocol.js`
  - public schemaへ optional `effort` (`1..=128 bytes`) を追加。
- `gateway/contract/routed-tools.json`
  - Rust public contract snapshotを再生成済み。

Focused verification:

- `local_agent::tests::task_profile_and_effort_bounds_fail_closed`: PASS
- `local_agent::tests::opencode_rejects_codex_reasoning_effort_before_executable_resolution`: PASS
- `local_agent::tests::codex_access_contract_has_no_bypass_or_caller_executable`: PASS
- `mcp::tests::public_tools_have_chatgpt_display_metadata`: PASS
- `mcp::tests::routed_gateway_contract_matches_checked_in_snapshot`: PASS / snapshot updated
- `npm test --prefix gateway`: 2/2 PASS
- `rustfmt --edition 2024 --check src/local_agent.rs src/mcp.rs`: PASS

Global `cargo fmt --all -- --check` はこの変更とは別の concurrent `session gc` 作業 (`src/cli.rs`) に未format差分があり FAIL。今回 scope の `src/local_agent.rs` は個別 rustfmt 済み。別作業ファイルは変更していない。

### Remaining work

1. `docs/usage.md` / `docs/usage.ja.md` に `profile` と `effort` の意味・Codex onlyであることを追記する。
2. current connected Temote runtimeを新buildへ切替後、実際に `local_agent_run(agent="codex", model=..., effort="max")` の live canaryを実施する。現在接続中runtime schemaはsource更新前のため、このturnでは live broker callを検証しない。
3. concurrent `session gc` 作業が完了/分離されたclean treeで `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets --locked`, `just sandboxed-check` を実行する。
4. full regression green確認後に本Issueを `issues/done` へ移動する。

## 2026-09-16 polishing update

`a8df54b` is on current `main`. Do not reimplement the schema/argv contract. Remaining work is only docs, full repository gate on a clean tree, and the rebuilt-runtime live canary; live evidence also updates the matrix.
