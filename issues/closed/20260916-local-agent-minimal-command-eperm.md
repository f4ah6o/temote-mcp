# `local_agent_run` の nested exec が `true` まで `Operation not permitted` になる

Status: folded into active local-agent EPERM tracker
Model: gpt-5.6-sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 developer workflow regression
Related:
- `issues/closed/20260916-local-agent-process-spawn-eperm.md`
- `issues/closed/20260916-sandbox-setup-failure-misclassified-as-child-failed.md`
- `src/local_agent.rs`
- `src/sandbox.rs`

## Observed behavior

`permission_mode=agent` の Temote session から `local_agent_run` で起動した agent 内部の process launch で、コマンド内容に依存せず同じ失敗になる。

最小 probe でも再現する。

```text
pwd
ls
true
```

いずれも同一系統の error:

```text
Operation not permitted (os error 1)
```

特に `true` まで失敗するため、Git / language toolchain / repository 内容に依存する失敗ではなく、nested child process の起動境界そのものが拒否されている可能性が高い。ただし原因となる kernel / namespace / seccomp / AppArmor / broker policy の層は未確定とし、決め打ちしない。

一方、同じ active session `temo` に対して Temote の通常 `execute` から次を実行すると、2026-09-16 の確認時点ではすべて exit 0 だった。

```text
pwd
ls
true
```

したがって、現時点の再現範囲は normal session の command sandbox 全体ではなく、`local_agent_run` 配下の nested process execution path に絞られる。

## Problem

`true` のような副作用も repository 依存もない最小 command を起動できない状態では、agent は source inspection、test、build、format、git inspection を一切行えず、`permission_mode=agent` の開発用途が成立しない。

また、上位の `local_agent_run` job が completed / exit 0 になり得る一方で、agent 内の全 child process が EPERM になる場合、job lifecycle だけでは実作業不能を検出しにくい。

## Expected behavior

`permission_mode=agent` の `local_agent_run` では containment と secret isolation を維持したまま、agent が workspace scope 内で通常の child process を起動できる。

最低限、次の smoke probe が通ること:

```text
true
pwd
ls
```

## Acceptance criteria

- [ ] Linux の `permission_mode=agent` session で `local_agent_run(agent=codex, access=read_only)` 内から `true` が exit 0 になる。
- [ ] 同条件で `pwd` と `ls` が実行でき、selected canonical cwd の範囲だけが見える。
- [ ] `workspace_write` でも同じ process-spawn smoke probe が通る。
- [ ] 外側の Temote `execute` と nested agent exec の failure domain を diagnostics で区別できる。
- [ ] sandbox/runtime setup failure、runner spawn failure、agent 内 child-process spawn failure を同一の generic error に潰さない。
- [ ] `true` / `pwd` / `ls` を用いた regression test または live acceptance coverage を追加する。
- [ ] protected `.git` / `.agents` / `.codex`、workspace containment、network/secret isolation を緩和しない。

## Scope note

`20260916-local-agent-process-spawn-eperm.md` が broader root-cause tracker。本 Issue は `true` まで失敗する最小再現と、outer `execute` は成功するという境界条件を固定し、修正後の smoke acceptance を明確化するための focused regression issue とする。

## 2026-09-16 polishing disposition

The `true` / `pwd` / `ls` smoke probe is folded into `issues/doing/20260916-local-agent-opencode-eperm.md` and Phase 0 of `issues/ROADMAP-20260916-agent-mode-main-only.md`.
