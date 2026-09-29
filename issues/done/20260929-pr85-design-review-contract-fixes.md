# PR #85 design review fixes: provisioning retry identity + RepositoryId grammar

Status: done — contract corrections delivered in this PR (docs only; runtime unchanged)  
Repository: `f4ah6o/temote-mcp`  
Source review: PR #85 `docs: make managed provisioning session-first` (merged at `5bdedb3`), reviewed HEAD `9afe5cd`, codex review comments  
Created: 2026-09-29 (Asia/Tokyo)

## 1. Findings and fix mapping

### F1 (P1) — managed provisioning retry identity undefined

Review location: `issues/open/20260929-session-first-managed-provisioning.md` §5.1 (comment id 4128280250).

Finding: the new managed `session_start` form had neither a caller-supplied `operation_id` nor an equivalent replay identity, although the operation performs repository preparation and workspace allocation and later sections require receipt-based idempotency. A caller that loses the response cannot distinguish "retry of the same request" from "a new Session for the same repository", so a blind retry can double-create Session / Workspace.

Fixes applied:

- `issues/open/20260929-session-first-managed-provisioning.md`
  - §5.1: added required `operation_id` to the concept API and `--operation-id` to the CLI examples; marked the new MCP/CLI forms as a future contract, not the current public `session_start(path=...)`.
  - §5.3 (new): full retry contract — `operation_id` issuance/scope (caller generates before first send, reuses on retry, binds to caller namespace + target host + operation kind; SessionId not required as a lookup prerequisite); request fingerprint (normalized request fields vs pinned execution state); durable Accepted receipt before the first session-owned side effect; lost-response / crash / concurrent-resend rules (`operation_conflict`, `reconciliation_required`, no implicit second Session); receipt retention (no unconditional reuse of an expired key as a new request).
  - §3: `OperationId` added to the identity-separation list.
  - §6: receipt/ownership ordering restated — no workspace allocation without a receipt.
  - §11: invariant 10 strengthened to name the caller-supplied key, durable receipt, and fail-closed outcomes.
  - §12 Phase S1: caller-supplied `operation_id` ownership + Accepted receipt before the first side effect + pinned revision recorded on the receipt.
  - §14 S3a: clarified that the caller surface exposes the retry semantics while receipt durability is established in S1, not deferred to S3a.
  - §15: added the retry acceptance bullet plus the eight scenario-level acceptance test specifications (lost response, concurrent resend, conflicting resend, crash after Accepted receipt, crash before Completed receipt, retry after remote base update, cross-caller key reuse, explicit new key). These are test specifications, not executed-test claims.
- `issues/open/20260927-instruction-side-bare-repo-provisioning.md`
  - §4: `operation_id` added to the composed `session_start` example; the operation's retry semantics now reference parent §5.3, with `ensure_repository` participating in the session provisioning operation's receipt.
  - §8 P1: idempotency bullet restated in terms of the shared contract.

### F2 (P2) — RepositoryId component grammar not inherited

Review location: `issues/polished/20260929-s0a-typed-session-source-contract.md` §3 (comment id 4128280255).

Finding: S0a's validation rules enumerated empty/absolute/traversal/query/fragment cases but did not inherit the F1 component grammar `^[A-Za-z0-9][A-Za-z0-9._-]*$` for each host/owner/name component, so percent escapes, backslashes, whitespace, and other invalid characters were not clearly rejected at the typed boundary that feeds RepositoryStore path components.

Fixes applied:

- `issues/polished/20260929-s0a-typed-session-source-contract.md`
  - §3: explicit grammar inheritance for host/owner/name and the supplied default host; re-validation after the `.git` strip; raw-input forbidden-structure checking before parser folding; allowed normalizations limited to host case, `github.com` owner/name case, and one terminal `.git` strip; no implicit repair (no trim-to-accept, no backslash conversion, no percent-decode).
  - §3 "Checked entry points": `RepositoryId` may only be produced through the validating parser or equivalent checked constructor; serde and other entry paths enforce the same invariants (contract hardening, not a claim of an existing Deserialize bug).
  - §8: negative test specifications 16–23 (encoded separators `%2F` / `%5C` / `%20`, literal backslash, leading/trailing/internal whitespace, tab/newline/NUL/control characters, dot and dot-dot components including inside URL forms, invalid default host, name empty after `.git` strip, malformed serde input) and property specifications 24–27 (notation-variant identity, grammar conformance of successful parses, serde round-trip preservation, no silent conversion to a different identity).
  - §11: matching acceptance items.
  - Revision note records that the implementation merged in PR #86 predates this strengthened contract (see §3 below).

## 2. Boundaries kept

- Session-first / jj-first / named-root-as-admission directions unchanged.
- No Git silent fallback introduced; ExistingWorkspace and existing dirty checkout preservation unchanged.
- F1/V2 done contracts referenced as authority, not weakened (grammar inherited as-is, not relaxed).
- Docs only: no runtime / wire / schema / lifecycle / persisted Session / tool-count changes in this PR.
- New MCP/CLI forms are documented as future contract examples, not current public features.

## 3. Known residual (outside this PR's docs-only scope)

- `src/session_source.rs` (merged via PR #86) predates the strengthened §3 contract: its component check does not yet enforce the full F1 grammar (e.g. percent escapes currently parse) and serde derives do not enforce the §3 invariants. A conformance change belongs to a follow-up implementation packet; this PR deliberately contains no runtime changes.
- The §5.3 retry contract has no runtime implementation yet; it lands with the S1+ provisioning packets.

## 4. Verification

- `git diff --check`: clean.
- Changed files: `issues/open/20260929-session-first-managed-provisioning.md`, `issues/polished/20260929-s0a-typed-session-source-contract.md`, `issues/open/20260927-instruction-side-bare-repo-provisioning.md`, this record.
- NOT RUN: Rust unit/integration tests, host/macOS gates, provisioning E2E — docs-only change; the added test lists are acceptance specifications for implementation packets, not executed tests.
