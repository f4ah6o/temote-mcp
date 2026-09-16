# Normal Temote session から repository の Linux sandbox gate を実行できない

Status: open
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 developer workflow friction
Type: development / test infrastructure

## Observed

`/home/hirohito-fujita/src/local-mcp` の通常 `permission_mode=agent` Temote session (`temo`) で、変更前 baseline の `cargo test --quiet` を実行すると Linux sandbox tests が 8 件失敗した。

確認した失敗は2種類に集約される。

1. sandbox test fixture が `/var/tmp` に temporary directory を作ろうとし、外側の Temote sandbox では read-only filesystem のため失敗する。
2. nested sandbox fixture が `/usr/bin/bwrap` に対して `Linux sandboxing requires a root-owned, non-writable /usr/bin/bwrap` を返し、process-boundary test を開始できない。

2026-09-16 の baseline 結果:

- library tests: 101 tests started
- 93 passed / 8 failed
- failure examples:
  - `linux_linked_worktree_git_operation_is_supported`
  - `linux_network_and_child_hardening_are_restricted`
  - `linux_local_agent_seccomp_allows_runtime_stream_pair_only`
  - `linux_filesystem_policy_allows_only_workspace_explicit_and_tmp_writes`
  - `linux_local_agent_profile_bounds_workspace_writes`
  - `service_account_private_pid_namespace_hides_host_peer_environments`
  - `linux_normal_git_metadata_is_read_only_but_run_git_can_commit`
  - `independent_processes_cannot_inspect_sensitive_supervisor_or_cli`

この失敗は activity S05 実装前に再現したため、activity 変更による回帰ではない。

同じ normal session では、activity S05/S06 の Unix socket fixture も外側 sandbox の制約で再実行できなかった。test-only state/socket path を process-private `/tmp` に隔離すると state lock の read-only failure は解消したが、`UnixStream::connect` / local session socket probe 自体が `Operation not permitted (os error 1)` になった。

- `cargo test --bin temote-mcp activity_ingress`: 1 passed / 6 failed。6件は socket connect/probe の `EPERM`。
- `cargo test --bin temote-mcp activity_producer`: 2 passed / 3 failed。pure bounded-queue / fixed-error tests は通り、3件の Unix socket fixture が `EPERM`。

このため normal Temote developer session では、nested sandbox tests だけでなく local Unix socket を使う process/runtime integration tests も host-level acceptance と分離する必要がある。未実行・環境 block を PASS に読み替えない。

## Goal

Temote MCP 自身を Temote の normal sandboxed developer session から開発するときにも、security coverage を弱めずに repository の標準 gate を実行・解釈できる supported path を用意する。

## Constraints

- Linux sandbox policy、network deny、Git metadata protection、PID isolation の実際の coverage を単に skip して green にしない。
- nested sandbox が本質的に実行不能な環境では、repository-local tests と host-level/process-boundary tests を明確に分離し、未実行 coverage を PASS と表示しない。
- `/var/tmp` や `/usr/bin/bwrap` の security precondition を開発 convenience のために緩めない。
- normal Temote session の sandbox/path/network 境界を広げない。

## Investigation

1. `/var/tmp` を必要とする fixture が本当に host `/var/tmp` を必要とするか、session 内の safe temporary root へ切り替え可能か確認する。
2. nested `bwrap` が outer sandbox 内で成立しない理由を、ownership/mount/userns/AppArmor のどこで拒否されているか分類する。
3. repository の標準 `cargo test` から、repository-local deterministic coverage と privileged/host-level acceptance coverage をどう分けるべきか決める。
4. Temote developer broker から host-level gate を安全に起動する専用 structured operation が必要か評価する。raw command / yolo への逃げ道にはしない。
5. local Unix socket fixture を必要とする integration tests を、normal sandbox 内で安全に実行可能にするか、host-level gate として明示的に実行する supported path を決める。

## Acceptance criteria

- [ ] normal Temote developer session で実行可能な deterministic test subset が明示され、activity 等の通常変更で regression gate として使える。
- [ ] nested sandbox が必要な Linux security tests は、対応 host で実行される gate が残り、未実行時は明確に NOT RUN / environment blocked と分かる。
- [ ] `/var/tmp` read-only による fixture failure を product regression と誤判定しない supported workflow がある。
- [ ] `/usr/bin/bwrap` の production security requirement は維持される。
- [ ] Unix socket integration coverage は対応 host-level gate で維持され、normal sandbox で `EPERM` の場合に product regression と誤判定しない。
- [ ] `AGENTS.md` / development docs の gate 記述が実際に実行可能な経路と一致する。
- [ ] CI は security coverage を失わず green を維持する。
