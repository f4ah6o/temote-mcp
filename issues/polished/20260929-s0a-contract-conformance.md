# S0a conformance: RepositoryId grammar + checked entry points

- Status: ready for implementation
- Repository: `f4ah6o/temote-mcp`
- Parent: `issues/polished/20260929-s0a-typed-session-source-contract.md`
- Related: `issues/open/20260929-session-first-managed-provisioning.md`, `issues/done/20260929-pr85-design-review-contract-fixes.md`, `issues/done/20260925-f1-repository-store-workspace-contract.md`
- Created: 2026-09-29 (Asia/Tokyo)
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
