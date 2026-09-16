# normal sandbox の direct Codex CLI が PATH alias 初期化で read-only warning を出す

Status: superseded by bounded implementation packet
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P2 developer workflow friction
Type: Codex / developer environment / sandbox

## Observed

active normal Temote session `temo` で direct CLI の存在確認として次を実行した。

```text
codex --version
```

command 自体は exit 0 で成功した。

```text
codex-cli 0.147.0
```

しかし stderr に毎回次の warning が出る。

```text
WARNING: proceeding, even though we could not create PATH aliases: Read-only file system (os error 30)
```

実体確認:

```text
which codex
/home/hirohito-fujita/.vite-plus/bin/codex

readlink -f /home/hirohito-fujita/.vite-plus/bin/codex
/home/hirohito-fujita/.vite-plus/0.3.1/bin/vp
```

host 側には Codex state path が存在する。

```text
/home/hirohito-fujita/.codex
/home/hirohito-fujita/.codex/tmp
/home/hirohito-fujita/.codex/tmp/arg0
```

通常 sandbox は HOME を保持する一方、workspace 外の host state は writable にしないため、Codex の startup-time alias/state 初期化と sandbox contract が衝突している可能性が高い。command は成功するため機能停止ではないが、正常な probe/test の stderr に environment warning が混ざり、実際の product/test warning と区別しづらい。

既存 `20260911-local-agent-vp-installed-codex-runtime.md` は `local_agent_run` 内で Vite+ launcher/dependency closure を可視化する問題を追跡している。本 issue は ordinary `execute` から direct Codex CLI を起動したときの mutable startup state / warning に限定する。

## Goal

normal sandbox の filesystem boundary を広げずに、supported Codex CLI probe/execution が expected startup state を安全な private writable location に持てる、または warning を operator が明確に environment friction と判定できるようにする。

## Investigation

1. Codex 0.147.0 が PATH alias 初期化時に実際に write しようとする path を bounded diagnostic で特定する。
2. Vite+ multicall launcher 固有か standalone Codex でも同じかを分離する。
3. local-agent の isolated `AgentState` と ordinary command sandbox の state policy を比較し、共有可能な private-state abstraction があるか確認する。
4. `CODEX_HOME` 等を切り替える場合は、既存 auth/config を意図せず失う・複製する・secret を temporary state にコピーする問題がないか確認する。
5. startup warning suppressionだけを目的に host `~/.codex` 全体を writable にしない。

## Security constraints

- host `~/.codex`、`~/.vite-plus`、credential/config 全体への broad write permission を追加しない。
- existing Codex authentication material を worktree や ordinary temp output にコピーしない。
- sandbox network/path containment を変更しない。
- warning を単に stderr filter で隠して root cause を見えなくしない。

## Acceptance criteria

- [ ] normal sandbox で supported Codex version/status probe が read-only filesystem warning を出さない、または fixed environment classification として明確に返る。
- [ ] 必要な mutable state path が特定され、最小の private writable scope に限定される。
- [ ] Vite+ managed Codex と standalone Codex の双方について expected behavior を fixture で固定する。
- [ ] host Codex credentials/config/state への write capability は増えない。
- [ ] `local_agent_run` の既存 isolated state / auth import contract を回帰させない。

## 2026-09-16 polishing disposition

Implementation work is superseded by `issues/polished/20260916-codex-direct-cli-private-state.md`, which narrows this investigation to one bounded state/fixture change.
