# `local_agent_run` で OpenCode / Codex child process の spawn が EPERM になる

Status: folded into active local-agent EPERM tracker
Model: gpt-5.6-sol
Created: 2026-09-16
Updated: 2026-09-16
Priority: P1 developer workflow regression
Related:
- `issues/closed/20260911-local-agent-vp-installed-codex-runtime.md`
- `src/local_agent.rs`
- `src/sandbox.rs`

## Observed behavior

`gh-git` の public repository (`/home/hirohito-fujita/src/gh-git`) を `permission_mode=agent` の Temote session から実装中に再現した。

### OpenCode

`local_agent_run`:

```text
agent=opencode
model=opencode-go/deepseek-v4.1-flash
access=workspace_write
```

を2回実行したが、どちらも job を作成する前に次で失敗した。

```text
Error: Unexpected error
EPERM : failed to spawn process
```

session はその後も `active`。`job_list` に OpenCode job は作成されていなかったため、同一処理の重複実行は発生していない。

### Codex

代替として `local_agent_run(agent=codex, access=workspace_write)` を実行すると job 自体は `running` → `completed (exit 0)` になったが、Codex 内部のすべての process launch が拒否された。

観測した error:

```text
Failed to create unified exec process: Operation not permitted (os error 1)
```

`pwd`、`git status --short`、`ls` など read-only command でも同じだった。

Codex job の stderr には Cloudflare MCP の OAuth `AuthRequired` も出たが、process spawn EPERM とは分離して扱う。今回の実装不能の直接原因は child process を一つも起動できないこと。

## Expected behavior

`permission_mode=agent` の `local_agent_run` では、sandbox/path containment を維持したまま、対応 agent が workspace 内で必要な child process を起動できる。

特に次が可能であること:

- OpenCode runner 自体を spawn できる
- Codex が `pwd` / `git status` / language toolchain 等の通常 child process を workspace scope 内で起動できる
- `workspace_write` は workspace 外への write を許可しない
- public MCP endpoint から yolo を要求しない

## Acceptance criteria

- [ ] Linux の `permission_mode=agent` session で OpenCode `local_agent_run` が job 作成前の `EPERM` にならない。
- [ ] 同条件の Codex job 内で `pwd` / `git status --short` / test runner が実行できる。
- [ ] nested user namespace / AppArmor / seccomp / sandbox policy のどこで拒否されたかを error に分類して返す。
- [ ] runner spawn failure と、agent 内 child-process spawn failure を別の error class として識別できる。
- [ ] `read_only` / `workspace_write` の containment、protected metadata、secret isolation は維持する。
- [ ] OpenCode / Codex の正常系 regression test を追加する。
- [ ] 実 host での live acceptance を行い、`session_info` / job state と合わせて記録する。

## Notes

`20260911-local-agent-vp-installed-codex-runtime.md` は Vite+ launcher dependency closure と live verification を主対象としている。今回の OpenCode の runner-level `EPERM` と Codex の generic child-process `EPERM` が同一原因かは未確定なので、原因を決め打ちせず別 issue として追跡する。

## 2026-09-16 polishing disposition

Source-side socketpair and failure-classification fixes are already on `main`. Rebuilt-runtime acceptance is owned by `issues/doing/20260916-local-agent-opencode-eperm.md`; this broader duplicate is closed.
