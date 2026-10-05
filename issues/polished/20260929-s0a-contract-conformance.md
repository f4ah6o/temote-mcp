# S0a conformance: RepositoryId grammar + checked entry points

Status: polished
Model: unknown
Created: 2026-09-29
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Make the merged S0a RepositoryId implementation enforce the inherited grammar at every construction entry point.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Make the merged S0a RepositoryId implementation enforce the inherited grammar at every construction entry point.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 6. Non-goals

- wiring `RepositoryId` / `SessionStartSpec` into provisioning (S1+)
- RepositoryStore ensure/fetch, workspace allocation, jj provisioning
- changing public `session_start(path=...)`, MCP schema/tool count, or lifecycle
- relaxing the F1 store grammar

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Fixed design from source

**3. Scope**

- make `validate_component()` (or the equivalent construction path) enforce the inherited grammar `^[A-Za-z0-9][A-Za-z0-9._-]*$` for `host` / `owner` / `name` and for the supplied default host
- keep the allowed normalizations exactly as contracted: host case, `github.com` owner/name case, one terminal `.git` strip — no implicit repair of invalid input
- route serde and any other `RepositoryId` construction through the same invariants (checked constructor, custom `Deserialize`, or validation-on-decode — implementation's choice); no unchecked materialization path
- re-validate the name after the `.git` strip; keep the raw source and the normalized components distinct
- add the negative and property unit tests listed in the S0a packet §8 items 16–27

## 受け入れ条件

Complete source criteria from “4. Acceptance” (unchecked items remain unverified):

- [ ] `host` / `owner` / `name` and the used default host satisfy `^[A-Za-z0-9][A-Za-z0-9._-]*$`
- [ ] percent escapes, backslashes, whitespace, and NUL/control characters are rejected
- [ ] malformed `RepositoryId` serde input is rejected
- [ ] checked constructor / parser / serde enforce identical invariants
- [ ] GitHub case normalization is preserved
- [ ] serde round-trip of a normalized `RepositoryId` preserves value and identity
- [ ] handling of the terminal `.git` suffix does not conflate the raw source with normalized components
- [ ] no change to current `session_start(path=...)`, MCP schema/tool count, or session lifecycle

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd gateway && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.

## 既存設計・履歴

- Historical Status: ready for implementation
- Repository: `f4ah6o/temote-mcp`
- Parent: `issues/polished/20260929-s0a-typed-session-source-contract.md`
- Related: `issues/open/20260929-session-first-managed-provisioning.md`, `issues/done/20260929-pr85-design-review-contract-fixes.md`, `issues/done/20260925-f1-repository-store-workspace-contract.md`
- Historical Created: 2026-09-29 (Asia/Tokyo)
- Observed baseline: `main` `d3b7d53ae3b10caa7f11f42adb1e506a0800d2fe` (includes the merged S0a implementation from PR #86)

## 1. Goal

Bring the merged `src/session_source.rs` into conformance with the strengthened §3 contract of `issues/polished/20260929-s0a-typed-session-source-contract.md` (PR #85 review follow-up: inherited F1 component grammar + checked entry points).

This packet is contract-only: it defines the conformance work; it does not implement it.

## 2. Static residue at `d3b7d53`

Read of `src/session_source.rs` (static inspection only — no runtime tests were executed for this review):

- `validate_component()` is a deny-list: it rejects empty components, whitespace, `.`/`..`, and `/ \ @ : ? #`, but does not enforce the F1 allow-grammar `^[A-Za-z0-9][A-Za-z0-9._-]*$`. For example, `repo%2Fother` currently parses because `%` is not on the deny-list; other out-of-grammar characters are likewise accepted.
- `RepositoryId` derives `Deserialize` and exposes public fields. `deny_unknown_fields` constrains field names only — it does not validate `host` / `owner` / `name` values, so serde input and struct literals can materialize identities that never passed the parser's validation/normalization.

## 3. Scope

- make `validate_component()` (or the equivalent construction path) enforce the inherited grammar `^[A-Za-z0-9][A-Za-z0-9._-]*$` for `host` / `owner` / `name` and for the supplied default host
- keep the allowed normalizations exactly as contracted: host case, `github.com` owner/name case, one terminal `.git` strip — no implicit repair of invalid input
- route serde and any other `RepositoryId` construction through the same invariants (checked constructor, custom `Deserialize`, or validation-on-decode — implementation's choice); no unchecked materialization path
- re-validate the name after the `.git` strip; keep the raw source and the normalized components distinct
- add the negative and property unit tests listed in the S0a packet §8 items 16–27

## 4. Acceptance

- [ ] `host` / `owner` / `name` and the used default host satisfy `^[A-Za-z0-9][A-Za-z0-9._-]*$`
- [ ] percent escapes, backslashes, whitespace, and NUL/control characters are rejected
- [ ] malformed `RepositoryId` serde input is rejected
- [ ] checked constructor / parser / serde enforce identical invariants
- [ ] GitHub case normalization is preserved
- [ ] serde round-trip of a normalized `RepositoryId` preserves value and identity
- [ ] handling of the terminal `.git` suffix does not conflate the raw source with normalized components
- [ ] no change to current `session_start(path=...)`, MCP schema/tool count, or session lifecycle

## 5. Dependency

This conformance fix must land **before** S1+ provisioning packets wire `SessionStartSpec` / `RepositoryId` into managed `session_start`, so the typed boundary feeding RepositoryStore path components is never weaker than the F1 store contract.

## 6. Non-goals

- wiring `RepositoryId` / `SessionStartSpec` into provisioning (S1+)
- RepositoryStore ensure/fetch, workspace allocation, jj provisioning
- changing public `session_start(path=...)`, MCP schema/tool count, or lifecycle
- relaxing the F1 store grammar
