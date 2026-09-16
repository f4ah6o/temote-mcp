# Activity `supervisor_upgrade` 計測を production upgrade coordinator と接続する

Status: done
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 dependency
Type: activity / upgrade integration

## Observed

`issues/doing/20260914-local-activity-viewer.md` S15 の prepared implementation (`5be7782`) は、production の `upgrade_coordinator` / remote `upgrade_apply` surface が存在する前提で `activity_runtime::try_emit_upgrade` を接続している。

現在の `main` には durable `upgrade_transaction` state machine と local `temote-mcp upgrade` / same-PID supervisor handoff は存在するが、prepared branch が参照する `src/upgrade_coordinator.rs` と remote `upgrade_apply` production entrypoint はまだ存在しない。

S15 の permission/restart/crash/auto-restart/forget lifecycle は現行 main に移植できた。一方、upgrade activity adapter だけを network build に残すと production caller がなく dead-code warning になり、`cargo clippy --all-targets -- -D warnings` を壊す。そのため、upgrade rebinding/delivery helper は当面 `#[cfg(test)]` に限定し、production surface が統合されるまで未接続であることを明示する。

## Goal

production upgrade coordinator / remote upgrade admission が main に統合された時点で、S15 の `supervisor_upgrade` activity を durable transaction state と安全に接続する。

## Resolution (2026-09-16)

- detached production `upgrade_coordinator` と remote upgrade admission / response-flush commit barrier を current main working tree に統合した。
- durable transaction に source session identity を保持し、persisted phase transition 後に `supervisor_upgrade` activity を best-effort deliveryする production callerを接続した。
- old runtime identity (`session_id + process_id + started_at`) と一致しない recreated runtime への terminal delivery は fail closed のまま維持した。
- `cargo check --all-targets` は PASS。`upgrade_coordinator::tests::` は 6/6 PASS、`upgrade_transaction::tests::` は 40/40 PASS。

## Constraints

- activity のためだけに未統合の upgrade subsystem 全体を逆輸入しない。
- upgrade の既存 same-PID handoff、restore plan、control protocol 2、rollback semantics を変更しない。
- source session の `session_id + process_id + started_at` を durable identity として使い、credential/free-form data を永続化しない。
- exec 後の新 process から旧 broker generation に completion event を捏造しない。
- activity delivery failure は upgrade outcome を変更しない。
- CI は dead code を許容して green に見せるのではなく、production caller ができるまで production code path を公開しない。

## Acceptance criteria

- [ ] production `upgrade_apply` / coordinator の実 entrypoint が main に存在する。
- [ ] admission 時に initiating session identity と activity operation_id を bounded durable context として保存する。
- [ ] durable transaction state の write 成功後にのみ best-effort activity update を発行する。
- [ ] approval denied / invalid input / runtime unavailable / admission failure が fixed typed summary で terminal 化される。
- [ ] exec/handoff 後に旧 generation の偽 completion を生成しない。
- [ ] `try_emit_upgrade` が production caller から利用され、`#[cfg(test)]` 制限を外しても clippy dead-code warning がない。
- [ ] existing upgrade tests + activity lifecycle tests + clippy/check が PASS する。
