# PR #85 design review fixes: provisioning retry identity + RepositoryId grammar

- Status: done — contract corrections delivered in this PR (docs only; runtime unchanged)
- Repository: `f4ah6o/temote-mcp`
- Source review: PR #85 `docs: make managed provisioning session-first` (merged at `5bdedb3`), reviewed HEAD `9afe5cd`, codex review comments
- Created: 2026-09-29 (Asia/Tokyo)

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
- `issues/closed/20260927-instruction-side-bare-repo-provisioning.md`
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

- `src/session_source.rs` (merged via PR #86) predates the strengthened §3 contract: `validate_component()` is a deny-list that does not yet enforce the full F1 grammar (e.g. percent escapes currently parse), and the `Deserialize` derive / public fields leave construction paths that never pass the parser's invariants. Tracked by `issues/polished/20260929-s0a-contract-conformance.md`; that conformance fix must land before S1+ provisioning wires `SessionStartSpec` / `RepositoryId` into managed `session_start`. This PR deliberately contains no runtime changes.
- The §5.3 retry contract has no runtime implementation yet; it lands with the S1+ provisioning packets.

## 4. Verification

Verification target (fixed at record time):

- base (PR #87 base): `origin/main` `d3b7d53ae3b10caa7f11f42adb1e506a0800d2fe`
- merge-base (`git merge-base origin/main HEAD`): `d3b7d53ae3b10caa7f11f42adb1e506a0800d2fe`
- head (PR #87 head): `d9ff75079e0a072c2dc413d5dd7ca9a87b02d31e`
- working-tree diff: present — this record's own follow-up edits (verification-record rewrite, S0a status sections, new conformance packet) are uncommitted on top of `d9ff750`. The committed PR diff is `d3b7d53...d9ff750`; the combined review diff is the working tree vs `d3b7d53`.
- diff identifiers: committed PR diff `git diff origin/main...d9ff750` → SHA-256 `163215af8cb202e7be693ecbd317bbc4da082789c72efb69ed88f6a84e3025e9`; the follow-up diff and combined diff are re-identifiable from the SHAs above (follow-up patch + SHA-256 delivered with the session report).

Commands and results (`gh git` / gh-git extension is not installed on this machine — `gh git diff --check` would dispatch the same `git diff --check`, which is what ran):

- `git diff --check origin/main...HEAD` at `d9ff750` (before the whitespace fix): exit 2 — FAIL. 5 trailing-whitespace notices on added/changed lines (3 header lines in this file, 1 `Status:` line in each edited doc). Recorded as a failed check, not waived.
- Fix applied to the failing changed lines only: `Status:` lines stand as their own paragraph instead of the two-space hard break; this file's and the conformance packet's headers use a bullet list. No unrelated reformatting; pre-existing `  ` metadata lines are outside the diff. No check configuration was weakened.
- `git diff --check origin/main` after the fix — combined diff, including this record's own edits: exit 0 — PASS.
- `git diff --check` — working tree vs `d9ff750` (this follow-up's own diff): exit 0 — PASS.
- Prior "`git diff --check`: clean" line: removed. It was read from an empty working-tree diff after committing and never covered the PR diff; it is not carried forward as a verified result.
- NOT RUN: doc lint / link check (no such tooling in this repo); Rust unit/integration tests, host/macOS gates, provisioning E2E — docs-only change; the added test lists are acceptance specifications for implementation packets, not executed tests.

Changed files: `issues/open/20260929-session-first-managed-provisioning.md`, `issues/polished/20260929-s0a-typed-session-source-contract.md`, `issues/closed/20260927-instruction-side-bare-repo-provisioning.md`, `issues/polished/20260929-s0a-contract-conformance.md` (new conformance packet), this record.
