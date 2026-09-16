# Upgrade transaction lock unit test が並列 state scan と競合して flaky になる

Status: done
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P1 CI stability
Type: test isolation / development friction

## Observed

S15 activity + remote upgrade integration後に次を実行した。

```text
cargo test --bin temote-mcp upgrade_
```

結果は 70 tests 中 58 passed / 12 failed。11件は normal Temote outer sandbox の既知 Unix socket `EPERM` だが、`upgrade_transaction::tests::transaction_lock_is_exclusive_and_released_on_drop` は socket 非依存なのに並列実行時だけ失敗した。

失敗箇所:

```text
assert!(acquire_transaction_lock(&fixture.transaction.transaction_id).is_ok())
```

一方、同じ test を単独 single-thread で実行すると PASS した。

## Resolution (2026-09-16)

`transaction_lock_is_exclusive_and_released_on_drop` を既存の `state_scan_test_lock()` に参加させ、shared process-private transaction directory を走査・変更する他 test と直列化した。production flock/lock semantics は変更していない。

検証:

- 単独 test: PASS
- `cargo test --bin temote-mcp upgrade_transaction::tests::`: 40/40 PASS（並列）

```text
cargo test --bin temote-mcp upgrade_transaction::tests::transaction_lock_is_exclusive_and_released_on_drop -- --exact --test-threads=1
# 1 passed / 0 failed
```

`upgrade_transaction` tests は process-private state directory を共有し、state/transaction scan 系 test は既に `state_scan_test_lock()` で直列化されている。この exclusive-lock test だけ guard に参加していないため、別 test の `transaction_lock_is_held` probe が同じ directory を scan する瞬間と lock drop/reacquire が競合できる。

## Goal

upgrade transaction lock の product semantics は変更せず、shared test state を触る unit tests を同じ isolation protocol に参加させて CI flake を除去する。

## Constraints

- production flock semantics、owner-only mode、O_NOFOLLOW、lock lifetime は変更しない。
- test を ignore / retry / sleep で隠さない。
- test-only shared state serialization の範囲に留める。
- exact single-test と parallel filtered suite の両方で確認する。

## Acceptance criteria

- [ ] `transaction_lock_is_exclusive_and_released_on_drop` が `state_scan_test_lock()` の test isolation に参加する。
- [ ] exact single-thread test PASS。
- [ ] socket-dependent testsを除く parallel upgrade transaction testsで再現しない。
- [ ] production lock implementation diff はない。
- [ ] `cargo fmt --all -- --check` / clippy / diff check が PASS。
