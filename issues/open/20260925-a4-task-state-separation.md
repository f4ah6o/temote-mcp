# A4: execution / verification / delivery 状態の分離

Status: open
Model: unknown
Created: 2026-09-25
Updated: 2026-10-09
Branch: codex/20261005-complete-issues-fabric

## 概要

Preserve distinct execution, verification, and delivery states and close the remaining conformance and host-gate evidence for merged A4.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

A completed execution must never be mistaken for verification PASS or delivery completion.

## 目標

Preserve distinct execution, verification, and delivery states and close the remaining conformance and host-gate evidence for merged A4.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Preserved fixed contract: 2. Fixed decisions

### 2.1 状態の分離

task view (`*_task_get` / `task_list` の item) に以下の 3 field を追加する。既存 top-level の
`status` は execution status として互換のため残す (文字列・意味は変更しない)。

```json
{
  "task_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb",
  "status": "completed",
  "revision": 7,
  "generation": 2,
  "execution": {
    "id": "6a1c1f33-...",
    "generation": 2,
    "state": "completed"
  },
  "verification": {
    "status": "not_run",
    "stale": false,
    "target": null,
    "record_revision": null,
    "checked_at": null
  },
  "delivery": {
    "status": "not_started",
    "branch": null,
    "pull_request": null,
    "updated_at": null
  }
}
```

- `execution.id` は論理 task `task_id` と execution `generation` から決定的に導出する UUIDv5。
  `task_id` は振り直さない。同じ (task_id, generation) は常に同じ id を返す。
- `execution.state` は既存 backend status の写像であり、`waiting_input` / `unknown` /
  `reconciliation_required` も success / failure へ読み替えずそのまま保持する。
- `verification.status` は `not_run` / `passed` / `failed`。保存済み結果が現在の revision に
  束縛されていない場合は `not_run` とし、`stale: true` と保存内容 (`target` /
  `record_revision` / `checked_at`) で履歴を示す。旧 PASS を現在の PASS として表示しない。
- `delivery.status` は `not_started` / `pending` / `submitted` / `merged` / `closed` / `failed`。
  agent の report や Devin Cloud の `pull_requests` から勝手に導出しない。delivery operation
  が記録するまで `not_started` を維持する。

### 2.2 永続 record への追加

4 backend の `TaskRecord` に additive な optional field を追加する:

```rust
#[serde(default)]
verification: Option<VerificationRecord>,
#[serde(default)]
delivery: Option<DeliveryRecord>,
```

- 既存の `schema_version` を保持する（Devin Cloud は **3**、他 backend は **1**）。field は additive / optional で、旧 record は `None` として
  読める。この packet はまだ書き込み operation を持たないため、旧 binary が新 field を落とす
  silent data loss の危険も生じない。最初に verification / delivery を永続化する packet が
  その時点で schema version の扱い (fail closed 化するか) を再評価する。
- 保存型は `src/orchestration/outcome.rs` に定義し、backend は record field として保持する。
  backend store が唯一の source of truth のままで、第二の task store は作らない。

```text
VerificationRecord { status: "passed"|"failed", target, record_revision: u64, checked_at: u64 }
VerificationTarget = Commit{commit} | Snapshot{snapshot} | Unidentified{commit?, dirty}
DeliveryRecord     { status, branch?, pull_request?, updated_at: u64 }
```

- `VerificationTarget::Commit` は clean workspace の commit、`Snapshot` は dirty を含む内容の
  opaque snapshot id、`Unidentified` は「内容を識別できない」ため **never current**。
- `verification.record_revision` は記録時の task record revision。読取時に現在の revision と
  一致しない、または `Unidentified` の場合は current PASS と扱わない (fail closed)。

### 2.3 読取規則 (全 backend 共通)

- record なし → `not_run`, `stale: false`, 他 null。
- record あり / `record_revision == record.revision` / target が `Commit` or `Snapshot`
  → 保存された `passed` / `failed` を現在状態として表示。
- record あり / `record_revision != record.revision` → `status: not_run`, `stale: true`。
- record あり / target が `Unidentified` → `status: not_run`, `stale: true`。

validation は文字列 bound (256 bytes, NUL なし) と `record_revision` / `checked_at` /
`updated_at` > 0 のみ。secret をこの record に書かない。

### 2.4 変更しないもの

- 公開 tool schema、tool 名、`task_get` の `after_revision` / `not_modified` 応答、
  operation receipt / replay 応答、approval metadata、store の保存先と lock 順序。
- 既存 top-level `status` / `generation` / `revision` field の意味。
- `OperationOutcome` と `operation_view` (replay は record view ではないため 3 状態を追加しない)。

## 受け入れ条件

Complete source criteria from “5. Acceptance” (unchecked items remain unverified):

- [ ] 4 backend の `task_get` / `task_list` view に `execution` / `verification` / `delivery` が入る。
- [ ] `status: completed` の record で verification 未記録なら `verification.status == "not_run"`。
- [ ] `record_revision` が古い `passed` は `status: not_run` + `stale: true` で返り、現在 PASS と
      して表示されない。`Unidentified` target も同様。
- [ ] 旧 record (`verification` / `delivery` field なし) が `None` として読め、view が
      `not_run` / `not_started` になる。
- [ ] Devin Cloud の `pull_requests` から delivery state を導出しない。
- [ ] `waiting_input` / `unknown` / `reconciliation_required` が execution state として
      そのまま保持される。
- [ ] 既存の tool schema / `not_modified` / replay 応答 / approval metadata が変わらない。
- [ ] `cargo fmt` / `cargo test` / `cargo clippy -D warnings` / `cargo check --no-default-features` /
      `git diff --check` が通る (sandbox 内では `just sandboxed-check`、host gate は未実行と明記)。

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 8. Current-port validation

- `cargo fmt --all -- --check`: PASS.
- `git diff --check`: PASS.
- `cargo clippy --all-targets -- -D warnings`: PASS.
- `cargo check --no-default-features --all-targets`: PASS (existing feature-gated unused warnings).
- `cargo test --no-run`: PASS using `CARGO_INCREMENTAL=0 RUSTFLAGS="-C link-arg=-fuse-ld=bfd"`; the default lld link failed with undefined hidden symbols on this environment, including a retry without incremental compilation. No repository toolchain/linker policy was changed.
- Focused outcome/backend compatibility and Cloud observer/migration tests: PASS.
- Full `cargo test`: interrupted explicitly (exit 130) after nested Linux host sandbox tests blocked; NOT PASS.
- Repository-prescribed sandboxed test subset: PASS (`cargo test --lib --all-features --locked -- --skip sandbox::linux_tests`, and binary filters `activity_job`, `activity_coverage`, `upgrade_transaction::tests::`, `upgrade_coordinator::tests::`, `session_control::tests::session_gc`). Host-only sandbox/process acceptance remains NOT RUN to completion locally; CI must qualify it before merge.
- Live backend acceptance: NOT RUN. Gateway runtime/protocol code unchanged; no new gateway test run.

### Additional source test detail: 6. Validation commands

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
git diff --check
```

実 backend (Codex / OpenCode / Devin / Devin Cloud) の実接続 acceptance はこの packet に含めない。
mock / fixture だけで実サービス PASS を報告しない。

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.
- 2026-10-05: A4 implementation was merged in PR #93. This issue stays open for conformance review and host/CI evidence; merged code is not proof that every acceptance and live gate passed.
- 2026-10-09: Re-checked against main 2706754; unchecked criteria and live/acceptance gates remain open.

## 既存設計・履歴

> Historical Status: current-main port locally qualified; host-only gates / CI pending; replaces PR #55
Repository: `f4ah6o/temote-mcp`
Base: `main` `fada256`; branch: `integrate/a4-current-main`
Parent issue: `issues/open/20260924-temote-development-harness-restructure.md` (PR #47)
Prerequisites: A1 `51424a4` (orchestration core) / A2 `9377bc8` (typed request + capability) / A3 `3e342d6` (session-owned task_list)

## 1. Goal

backend の task record / view に **execution / verification / delivery を別状態として導入** し、
`status: completed` だけでは verification PASS にならないことを固定する。今回は状態契約と読取規則、
旧 record fixture、view 配線までとし、verification / delivery を書き込む operation は後続 packet
(E / D 系) に残す。

## 2. Fixed decisions

### 2.1 状態の分離

task view (`*_task_get` / `task_list` の item) に以下の 3 field を追加する。既存 top-level の
`status` は execution status として互換のため残す (文字列・意味は変更しない)。

```json
{
  "task_id": "0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb",
  "status": "completed",
  "revision": 7,
  "generation": 2,
  "execution": {
    "id": "6a1c1f33-...",
    "generation": 2,
    "state": "completed"
  },
  "verification": {
    "status": "not_run",
    "stale": false,
    "target": null,
    "record_revision": null,
    "checked_at": null
  },
  "delivery": {
    "status": "not_started",
    "branch": null,
    "pull_request": null,
    "updated_at": null
  }
}
```

- `execution.id` は論理 task `task_id` と execution `generation` から決定的に導出する UUIDv5。
  `task_id` は振り直さない。同じ (task_id, generation) は常に同じ id を返す。
- `execution.state` は既存 backend status の写像であり、`waiting_input` / `unknown` /
  `reconciliation_required` も success / failure へ読み替えずそのまま保持する。
- `verification.status` は `not_run` / `passed` / `failed`。保存済み結果が現在の revision に
  束縛されていない場合は `not_run` とし、`stale: true` と保存内容 (`target` /
  `record_revision` / `checked_at`) で履歴を示す。旧 PASS を現在の PASS として表示しない。
- `delivery.status` は `not_started` / `pending` / `submitted` / `merged` / `closed` / `failed`。
  agent の report や Devin Cloud の `pull_requests` から勝手に導出しない。delivery operation
  が記録するまで `not_started` を維持する。

### 2.2 永続 record への追加

4 backend の `TaskRecord` に additive な optional field を追加する:

```rust
#[serde(default)]
verification: Option<VerificationRecord>,
#[serde(default)]
delivery: Option<DeliveryRecord>,
```

- 既存の `schema_version` を保持する（Devin Cloud は **3**、他 backend は **1**）。field は additive / optional で、旧 record は `None` として
  読める。この packet はまだ書き込み operation を持たないため、旧 binary が新 field を落とす
  silent data loss の危険も生じない。最初に verification / delivery を永続化する packet が
  その時点で schema version の扱い (fail closed 化するか) を再評価する。
- 保存型は `src/orchestration/outcome.rs` に定義し、backend は record field として保持する。
  backend store が唯一の source of truth のままで、第二の task store は作らない。

```text
VerificationRecord { status: "passed"|"failed", target, record_revision: u64, checked_at: u64 }
VerificationTarget = Commit{commit} | Snapshot{snapshot} | Unidentified{commit?, dirty}
DeliveryRecord     { status, branch?, pull_request?, updated_at: u64 }
```

- `VerificationTarget::Commit` は clean workspace の commit、`Snapshot` は dirty を含む内容の
  opaque snapshot id、`Unidentified` は「内容を識別できない」ため **never current**。
- `verification.record_revision` は記録時の task record revision。読取時に現在の revision と
  一致しない、または `Unidentified` の場合は current PASS と扱わない (fail closed)。

### 2.3 読取規則 (全 backend 共通)

- record なし → `not_run`, `stale: false`, 他 null。
- record あり / `record_revision == record.revision` / target が `Commit` or `Snapshot`
  → 保存された `passed` / `failed` を現在状態として表示。
- record あり / `record_revision != record.revision` → `status: not_run`, `stale: true`。
- record あり / target が `Unidentified` → `status: not_run`, `stale: true`。

validation は文字列 bound (256 bytes, NUL なし) と `record_revision` / `checked_at` /
`updated_at` > 0 のみ。secret をこの record に書かない。

### 2.4 変更しないもの

- 公開 tool schema、tool 名、`task_get` の `after_revision` / `not_modified` 応答、
  operation receipt / replay 応答、approval metadata、store の保存先と lock 順序。
- 既存 top-level `status` / `generation` / `revision` field の意味。
- `OperationOutcome` と `operation_view` (replay は record view ではないため 3 状態を追加しない)。

## 3. Read / change scope

- `src/orchestration/outcome.rs` (新規): 状態型、validation、`execution_id` / view 関数、読取規則 test。
- `src/orchestration.rs`: `mod outcome;` の宣言のみ。
- `src/codex_app_server.rs` / `src/opencode_server.rs` / `src/devin_acp.rs` /
  `src/devin_cloud.rs`:
  - `TaskRecord` へ `verification` / `delivery` field 追加 (serde default)
  - `validate_record` から outcome validation を呼ぶ
  - `task_view` に `execution` / `verification` / `delivery` を追加 (`task_list_item` は
    `task_view` 経由のため自動で反映)
  - 旧 record fixture test と completed ≠ verification PASS test を追加
  - 既存 test の `TaskRecord` literal に `verification: None, delivery: None` を追加
- `docs/usage.md` / `docs/usage.ja.md`: Delegation and jobs に 3 状態の説明を追加。
- `CHANGES.md`: Unreleased に task state separation を追記。
- 本 issue ファイル。

## 4. Steps

1. 現状確認: 4 backend の `TaskRecord` / `task_view` / `validate_record` / literal / test を確認。
2. `src/orchestration/outcome.rs` を追加し、unit test で読取規則 (not_run default / current PASS /
   stale PASS / Unidentified / delivery default / execution id 安定性) を固定。
3. 4 backend の record / validation / view を配線。
4. 旧 record fixture test: 現行 record を serialize し `verification` / `delivery` を除いて
   deserialize して `None` として読めること、view が `not_run` / `not_started` になることを確認。
5. completed + report あり + verification なしで `not_run`、Devin Cloud では
   `pull_requests` が非空でも `delivery.status == not_started` を確認。
6. stale PASS: `record_revision` が現在 revision より古い `passed` record が `not_run` +
   `stale: true` で返ることを 4 backend で確認。
7. diff / gates を確認し、許可に従って commit / push / PR 作成。

## 5. Acceptance

- [ ] 4 backend の `task_get` / `task_list` view に `execution` / `verification` / `delivery` が入る。
- [ ] `status: completed` の record で verification 未記録なら `verification.status == "not_run"`。
- [ ] `record_revision` が古い `passed` は `status: not_run` + `stale: true` で返り、現在 PASS と
      して表示されない。`Unidentified` target も同様。
- [ ] 旧 record (`verification` / `delivery` field なし) が `None` として読め、view が
      `not_run` / `not_started` になる。
- [ ] Devin Cloud の `pull_requests` から delivery state を導出しない。
- [ ] `waiting_input` / `unknown` / `reconciliation_required` が execution state として
      そのまま保持される。
- [ ] 既存の tool schema / `not_modified` / replay 応答 / approval metadata が変わらない。
- [ ] `cargo fmt` / `cargo test` / `cargo clippy -D warnings` / `cargo check --no-default-features` /
      `git diff --check` が通る (sandbox 内では `just sandboxed-check`、host gate は未実行と明記)。

## 6. Validation commands

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
git diff --check
```

実 backend (Codex / OpenCode / Devin / Devin Cloud) の実接続 acceptance はこの packet に含めない。
mock / fixture だけで実サービス PASS を報告しない。

## 7. Integration boundaries (2026-10-02)

- Preserve current pending-interaction fields, producer epochs, observer leases and schema-3 start-fingerprint identity/migration.
- Observer-only metadata refresh does not invalidate verification while task revision is unchanged.
- Verification currently uses task record revision conservatively. BW1 semantic-revision stability remains a separate prerequisite for efficient waiting; the first verification writer must bind results to content identity and avoid invalidation from delivery-only mutations. This read/view packet adds no writer or test-success inference.
- Historical PR #55 test results do not qualify this port. Record new checks against this branch before delivery.

## 8. Current-port validation

- `cargo fmt --all -- --check`: PASS.
- `git diff --check`: PASS.
- `cargo clippy --all-targets -- -D warnings`: PASS.
- `cargo check --no-default-features --all-targets`: PASS (existing feature-gated unused warnings).
- `cargo test --no-run`: PASS using `CARGO_INCREMENTAL=0 RUSTFLAGS="-C link-arg=-fuse-ld=bfd"`; the default lld link failed with undefined hidden symbols on this environment, including a retry without incremental compilation. No repository toolchain/linker policy was changed.
- Focused outcome/backend compatibility and Cloud observer/migration tests: PASS.
- Full `cargo test`: interrupted explicitly (exit 130) after nested Linux host sandbox tests blocked; NOT PASS.
- Repository-prescribed sandboxed test subset: PASS (`cargo test --lib --all-features --locked -- --skip sandbox::linux_tests`, and binary filters `activity_job`, `activity_coverage`, `upgrade_transaction::tests::`, `upgrade_coordinator::tests::`, `session_control::tests::session_gc`). Host-only sandbox/process acceptance remains NOT RUN to completion locally; CI must qualify it before merge.
- Live backend acceptance: NOT RUN. Gateway runtime/protocol code unchanged; no new gateway test run.
