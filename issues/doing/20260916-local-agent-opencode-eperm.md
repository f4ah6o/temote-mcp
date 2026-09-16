# `local_agent_run(agent=opencode)` が executable 起動前後で EPERM になる

Status: doing / implementation landed; rebuilt-runtime live canary pending
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 developer workflow regression
Type: local agent / developer broker

## Observed

public repository `/home/hirohito-fujita/src/local-mcp` の active normal Temote session (`permission_mode=agent`, `yolo=false`) から OpenCode を実装 worker として使おうとした。

指定:

- agent: `opencode`
- model: `opencode-go/deepseek-v4.1-flash`
- cwd: repository root

`workspace_write` の S05 実装 task と、切り分け用の最小 `read_only` canary の両方が job/task を残さず同じエラーで失敗した。

```text
Error: Unexpected error

EPERM : failed to spawn process
```

一方で Temote の通常 `execute` から同じ resolved binary を直接起動した read-only diagnostic は成功した。

```text
which opencode
/home/hirohito-fujita/.opencode/bin/opencode

stat -Lc '%F %A %U:%G %n' /home/hirohito-fujita/.opencode/bin/opencode
regular file -rwxr-xr-x hirohito-fujita:hirohito-fujita /home/hirohito-fujita/.opencode/bin/opencode

/home/hirohito-fujita/.opencode/bin/opencode --version
1.18.31
```

したがって、この観測だけでは provider entitlement/model readiness の問題とは言えず、少なくとも `local_agent_run` 固有の process launch/sandbox/state path のどこかで EPERM が発生している。

## Goal

normal `agent` session から、検証済み OpenCode executable を `local_agent_run` の既存 bounded/sandboxed contract のまま起動できるようにする。

## Constraints

- raw executable/argv を MCP caller に公開しない。
- session roots、Git metadata、network、auth isolation、private agent state の境界を弱めない。
- `execute` で OpenCode 本体が起動できることを理由に、`local_agent_run` の sandbox を迂回しない。
- provider credential/model entitlement と process-spawn failure を混同しない。
- yolo への切替を回避策にしない。

## Investigation

1. `local_agent_run` の OpenCode adapter が spawn 前に作る private state/cache/home と sandbox profile を特定し、どの syscall/path で EPERM が返るか固定 classification で観測できるようにする。
2. executable discovery/revalidation は成功しているか、失敗が bwrap/userns/seccomp/filesystem policy のどこかを切り分ける。
3. read_only と workspace_write の両方で同じ EPERM になる理由を確認する。
4. raw provider outputやcredentialを出さず、operator-facing error を `failed to spawn process` より狭い固定分類へ改善する。
5. fake OpenCode fixture と実 binary canary の双方で regression coverage を追加する。

## Root cause classification (2026-09-16)

The Temote sandbox helper itself starts successfully and OpenCode 1.18.31 begins execution; the `EPERM : failed to spawn process` text is emitted by the OpenCode/Bun child and returns exit 1. This rules out executable discovery and the outer `temote-linux-sandbox` spawn as the failing layer.

The network-enabled Linux local-agent seccomp profile allowed only this Unix socketpair type:

```text
AF_UNIX, SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK, protocol 0
```

libuv's Unix process/stdio path creates connected socketpairs as `SOCK_STREAM|SOCK_CLOEXEC|flags`, where `flags` may be zero and nonblocking may be applied later. OpenCode's Linux executable embeds the libuv/Node process runtime. Therefore a normal blocking `SOCK_STREAM|SOCK_CLOEXEC` pair used by child-process plumbing was rejected with EPERM before provider/model readiness could be evaluated.

Reference implementation behavior:

- https://github.com/libuv/libuv/blob/v1.x/src/unix/process.c
- https://docs.libuv.org/en/v1.x/guide/processes.html#parent-child-ipc

## Implementation (2026-09-16)

- Keep `socket(AF_UNIX, ...)` denied for network-enabled local agents, so pathname/abstract host Unix sockets cannot be created for new connections.
- Allow exactly two connected `socketpair` forms, both `AF_UNIX`, stream, CLOEXEC, protocol 0:
  - `SOCK_STREAM|SOCK_CLOEXEC`
  - `SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK`
- Continue denying plain `SOCK_STREAM`, datagram pairs, other domains, non-zero protocols, and arbitrary socket types.
- Extend the existing nested sandbox fixture so both allowed forms must succeed while path/abstract host socket reachability remains denied.
- Add `noprop` property coverage (1,024 generated socket types) against the exact two-value allowlist, plus a direct seccomp BPF compilation test.

Repository-local verification:

- `cargo test --lib sandbox::linux::helper::tests::`: PASS, 8/8 including BPF generation.
- `cargo test --lib local_agent_socketpair`: PASS, 2/2 including 1,024-case PBT.
- OpenCode contract and imported-auth denial tests: PASS.
- `cargo clippy --all-targets -- -D warnings`: PASS.
- Nested local-agent runtime fixtures remain NOT RUN in this Temote session: the outer development sandbox makes `/var/tmp` read-only / bubblewrap unavailable. This is tracked separately in `20260916-normal-session-ci-sandbox-friction.md`.

The currently running Temote server predates this source change, so repeating `local_agent_run(agent=opencode)` against that process would only retest the old policy. The real OpenCode 1.18.31 canary remains pending after a rebuilt Temote runtime is active.

## Acceptance criteria

- [ ] active normal `agent` session で `local_agent_run(agent=opencode, access=read_only)` の最小 canary が child process を開始できる。
- [ ] `workspace_write` は既存 workspace write 境界の中だけで起動できる。
- [ ] OpenCode 1.18.31 の実 executable で process launch まで到達することを、provider/model entitlement と分離して確認できる。
- [ ] spawn failure は fixed/bounded classification と non-secret evidence で原因層を特定できる。
- [ ] Codex local agent、OpenCode fake adapter、auth isolation、sandbox tests が回帰しない。
- [ ] yolo、broad HOME exposure、raw argv/executable input を追加しない。
