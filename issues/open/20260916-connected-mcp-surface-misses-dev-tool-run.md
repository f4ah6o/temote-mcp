# connected MCP client surface に landed 済み `dev_tool_run` が現れない

Status: open
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 developer workflow friction
Type: MCP contract / deployment / client integration

## Observed

current repository source / docs / checked-in gateway contract では `dev_tool_run` が正式 surface として存在する。

- `src/mcp.rs`: `dev_tool_run` を tools/list schema と dispatch に登録
- `src/dev_tool.rs`: Cargo / Vite+ classifier と execution profile を実装
- `gateway/contract/routed-tools.json`: `dev_tool_run` を routed tool として保持
- `docs/usage.md` / `skills/temote-mcp/SKILL.md`: operator/agent usage を記載
- `issues/done/20260910-developer-execution-broker.md`: implementation complete と記録

しかし今回 ChatGPT から接続している Temote MCP action surface には、`session_*`, `execute`, `local_agent_run`, `git_*` 等は提供されている一方、`dev_tool_run` が利用可能 tool として現れていない。

その結果、Cargo/Vite+ のために実装済みの scoped cache/network capability を使えず、agent は raw `execute` へフォールバックする。raw execute は ordinary sandbox の network-disabled / Git-metadata-read-only contract のため、まさに broker が解消するはずだった開発摩擦が再発する。

active tool response の server info は `temote-mcp 2026.9.9` を返している。source/current main と connected runtime / connector schema のどこで drift しているかは未確定なので、原因を deployment lag、connector schema cache、server binary lag のいずれかに決め打ちしない。

2026-09-16 の追加確認では connector action discovery を exact query `dev_tool_run` で再実行したが、10件の関連 action が返る中に `dev_tool_run` 自体は含まれなかった。同じ discovery response には `local_agent_run`, `git_fetch`, `git_pull` 等は含まれている。したがって単なる UI 上の見落としではなく、少なくとも現在の connected action schema から `dev_tool_run` が欠落している。

さらに source/runtime drift を behavior でも確認した。current source `src/sandbox.rs` は ordinary sandbox command ごとに private cache root を作り、`XDG_CACHE_HOME=<private>/xdg` と `GOCACHE=<private>/go-build` を設定する。しかし active connected runtime の `execute` では次を返した。

```text
uv cache dir
/home/hirohito-fujita/.cache/uv

npm config get cache
/home/hirohito-fujita/.npm

go env GOCACHE GOMODCACHE GOPATH
/home/hirohito-fujita/.cache/go-build
/home/hirohito-fujita/go/pkg/mod
/home/hirohito-fujita/go
```

同 session を `session_restart` した後もこの behavior は変わらなかった。session restart は managed child session の再起動であり、Temote server binary / connected connector schema の rebuild・refresh ではないことが確認できる。したがって「repository main に修正済み」「session を restart 済み」だけでは connected runtime への反映証拠にならない。

同じ connected action discovery で、current source に実装済みの新規 Git/GitHub surface も未反映であることを確認した。

- exact query `git_push_tag`: 関連する既存 `git_push` / `git_commit` / `git_add` / `git_pull` / `git_fetch` は返るが `git_push_tag` 自体は返らない。
- exact query `github_workflow`: 0件。

この時点の current working source では `git_push_tag`, `github_workflow_dispatch`, `github_workflow_run_get` が tools schema / gateway contract に存在し repository-local deterministic gate も通っている。したがって connected client capability は source checkout の存在ではなく、deployed server + connector schema の実状態で判定する必要がある。

2026-09-16 後続確認では structured branch/worktree implementation が commit `72e8f97` (`feat: add structured Git branch and worktree operations`) として `origin/main` に反映済みになった後も、exact discovery `git_worktree_add` は `git_add` / `git_commit` / fetch/pull/push / `apply_patch` だけを返し、`git_branch_create`, `git_switch`, `git_worktree_add` 自体は connected surface に現れなかった。source commit/push 完了後にも connector/runtime refresh が別途必要であることの追加 evidence である。

## Goal

repository で supported と宣言し gateway parity test を通した public MCP tool が、実際の supported client connection でも同じ contract で discoverable / invokable になることを保証する。

## Investigation

1. active Temote server の real `tools/list` に `dev_tool_run` が存在するか確認する。
2. direct local MCP / authenticated HTTP / gateway / ChatGPT connector の各 surface で tool name + input schema fingerprint を比較する。
3. server binary version / source commit / gateway contract version / connector-registration version を bounded non-secret diagnostics で確認可能にする。
4. deploy/release 後に stale tool schema が残る lifecycle を確認する。
5. current source が未releaseなら、operator が「source にある = connected client から使える」と誤認しないよう runtime capability/version を明示する。

## Acceptance criteria

- [ ] current supported ChatGPT/remote MCP connection で `dev_tool_run` が discoverable になる。
- [ ] local stdio / HTTP / gateway / connector の tool-set parity を自動検証できる。
- [ ] parity check は tool name だけでなく exact input schema / public boundary も確認する。
- [ ] server/source/gateway/connector の version or contract fingerprint drift を operator が bounded diagnostic で判別できる。
- [ ] missing tool の場合に raw `execute` や yolo への危険な fallback を推奨しない。
- [ ] release/deployment pipeline に contract-parity acceptance を追加する。
