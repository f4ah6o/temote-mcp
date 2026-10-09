# NR1: physical cwd → named-root reverse resolution for local session start

Status: done
Model: unknown
Created: 2026-09-27
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Reverse-resolve local cwd to a named-root logical path with deterministic overlap handling.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The preserved design records a concrete remaining contract or defect; its implementation and verification have not been completed in this normalization pass.

## 目標

Reverse-resolve local cwd to a named-root logical path with deterministic overlap handling.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

### Preserved fixed contract: 2. Fixed decisions

- Logical identity = `root_name` + `relative_path` (parent §5); the canonical
  physical path remains host-local execution/diagnostic data.
- Nested/overlapping roots resolve deterministically (parent §6.4): pick the
  longest matching canonical root path.
- Equal-length ambiguity is eliminated at config load, not resolved at lookup:
  `NamedRoots::parse` (`src/named_roots.rs`) rejects a `TEMOTE_MCP_ROOTS`
  config where two distinct names canonicalize to the same physical directory
  — startup config error naming both roots. The logical root name is durable
  identity, so a same-directory duplicate must never resolve by map/iteration
  order (parent forbids that fallback). `reverse_resolve` additionally fails
  (never picks) if more than one root matches at equal depth — unreachable
  once parse rejects, kept as defense-in-depth.
- Symlinked cwd is canonicalized before matching (reuse
  `config::canonical_directory`), so a symlink cannot escape into a root
  (parent §13 "Symlink escape").
- The integrated NR2–NR4 packet supersedes the earlier Phase B warning:
  new normal starts outside all roots fail closed. Legacy restart keeps its
  verified stored cwd. The original Phase B design remains in the history below.
- Sessions started by explicit logical path (`roots.resolve` at
  `src/supervisor.rs:446`) are unchanged.

## 受け入れ条件

Complete source criteria from “5. Acceptance” (unchecked items remain unverified):

- [x] A compat `start` whose cwd sits inside a named root records the logical root-relative identity.
- [x] Nested/overlapping roots resolve deterministically (longest match), and equal-length ambiguity is impossible: duplicate canonical roots are rejected at parse, and `reverse_resolve` errors rather than picks on any residual equal-depth match.
- [x] A symlinked path inside a root resolves; a symlink escaping all roots fails closed under NR2–NR4, never mis-bound.
- [x] Outside-roots or unconfigured-root new normal starts fail closed under the subsequent NR2–NR4 packet; legacy restart scope remains unchanged.
- [x] Existing explicit-logical-path start is untouched.

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 6. Validation commands

- `cargo test --bin temote --all-features --locked named_roots`
- `cargo test --bin temote --all-features --locked supervisor`
- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `just sandboxed-check`
- macOS path-canonicalization differences: host gate, NOT RUN here.

## リスク

- Preserve session ownership, canonical scope, approval, bounded evidence, and fail-closed routing; do not reinterpret an unknown state as success.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 検証記録

- 2026-10-06: longest canonical root, duplicate/equal-depth rejection, symlink containment, explicit path and legacy restart fixtures PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/named_roots.rs; src/supervisor.rs
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.


- 2026-10-06: The same authorized workstream includes NR2–NR4, which supersedes the Phase B warn-only start behavior. The historical Phase B design below is preserved; the active criteria above describe the integrated fail-closed behavior.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.

## 既存設計・履歴

> Historical Status: ready (revised 2026-09-27 per PR #73 review: duplicate-canonical-root
tie policy fixed at parse time).
Repository: `f4ah6o/temote-mcp`
Branch / observed HEAD: `main` `c305e41`
Parent issue: `issues/open/20260926-named-root-workspace-identity.md`
Prerequisites: NR0 verified largely satisfied on `main` — `NamedRoots`
(`src/named_roots.rs`) already centralizes `TEMOTE_MCP_ROOTS` parsing and
`resolve()`; confirm no remaining ad-hoc parsing and note findings in the parent.

## 1. Goal

The compatibility local start path (`start_local_with_mode_with_environment`,
`src/supervisor.rs:~470`) no longer treats ambient physical cwd as workspace
identity: it reverse-resolves cwd through the named-root registry to a logical
`<root>/<relative>` identity, records it, and — per the parent's migration plan
(Phase A normalize → Phase B warn; Phase C hard fail is a later packet) — warns
rather than refuses when cwd is outside every root.

## 2. Fixed decisions

- Logical identity = `root_name` + `relative_path` (parent §5); the canonical
  physical path remains host-local execution/diagnostic data.
- Nested/overlapping roots resolve deterministically (parent §6.4): pick the
  longest matching canonical root path.
- Equal-length ambiguity is eliminated at config load, not resolved at lookup:
  `NamedRoots::parse` (`src/named_roots.rs`) rejects a `TEMOTE_MCP_ROOTS`
  config where two distinct names canonicalize to the same physical directory
  — startup config error naming both roots. The logical root name is durable
  identity, so a same-directory duplicate must never resolve by map/iteration
  order (parent forbids that fallback). `reverse_resolve` additionally fails
  (never picks) if more than one root matches at equal depth — unreachable
  once parse rejects, kept as defense-in-depth.
- Symlinked cwd is canonicalized before matching (reuse
  `config::canonical_directory`), so a symlink cannot escape into a root
  (parent §13 "Symlink escape").
- Outside-all-roots cwd keeps working in this slice but emits a recorded
  deprecation warning and marks the session metadata (Phase B); hard fail is
  deferred to NR3.
- Sessions started by explicit logical path (`roots.resolve` at
  `src/supervisor.rs:446`) are unchanged.

## 3. Read / change scope

- `src/named_roots.rs`: add `NamedRoots::reverse_resolve(&self, cwd) ->
  Option<RootRelativePath>`-style API plus tests (209+ has `validate_root_name`;
  parser tests live in the same file).
- `src/supervisor.rs`: `start_local_with_mode_with_environment` (~line 470,
  currently calls `config::canonical_directory` and stores `None` for
  `logical_path`), `start_resolved` (~line 500) signature already takes an
  optional logical path — populate it from the reverse resolution.
- `src/main.rs`: the compat `start`/CLI path calling the local-start API
  (TEMOTE_MCP_ROOTS doc at ~line 450, ~847–860) — route through the same
  resolver and surface the warning.
- Session metadata persistence for `root_name`/`root_relative_path` belongs to
  NR2; in this packet store the resolved logical path only where a field
  already exists (`logical_path` param of `start_resolved`) — do not add new
  persisted fields here.

## 4. Steps

1. Verify NR0 residual: grep for direct `TEMOTE_MCP_ROOTS` reads outside
   `named_roots`/`main` doc text (`managed_worktree.rs:1023` uses
   `NamedRoots::from_env` — confirm it is the only extra parse site).
2. Implement `reverse_resolve` with canonicalization + longest-match
   semantics and the equal-depth ambiguity error; add the
   duplicate-canonical-root rejection to `NamedRoots::parse`.
3. Wire it into the local-start path; emit the Phase-B warning when no root
   matches.
4. Tests: inside-root cwd, nested roots, symlinked cwd, outside-roots cwd
   (warn, still works), root mapping changed after session start, and a config
   with two names canonicalizing to the same directory rejected at parse.
5. `just sandboxed-check`; host/macOS path checks recorded as NOT RUN.

## 5. Acceptance

- [ ] A compat `start` whose cwd sits inside a named root records the logical
      root-relative identity.
- [ ] Nested/overlapping roots resolve deterministically (longest match), and
      equal-length ambiguity is impossible: duplicate canonical roots are
      rejected at parse, and `reverse_resolve` errors rather than picks on any
      residual equal-depth match.
- [ ] A symlinked path inside a root resolves; a symlink escaping all roots is
      treated as outside-roots (warn), never mis-bound.
- [ ] Outside-roots cwd still starts (Phase B warn only) — no behavior break.
- [ ] Existing explicit-logical-path start is untouched.

## 6. Validation commands

- `cargo test --bin temote-mcp --all-features --locked named_roots`
- `cargo test --bin temote-mcp --all-features --locked supervisor`
- `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`
- `cargo check --no-default-features --all-targets`
- `just sandboxed-check`
- macOS path-canonicalization differences: host gate, NOT RUN here.

## 7. Delivery authorization

One feature branch + one PR to `main`. Keep migration-Phase-C hard-fail behavior
out of this packet.

## 8. Completion report

(to be filled by the implementing packet run)
