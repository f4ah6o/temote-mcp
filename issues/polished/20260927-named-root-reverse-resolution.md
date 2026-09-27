# NR1: physical cwd → named-root reverse resolution for local session start

Status: ready
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
  longest matching canonical root path; document and test the tie-break.
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
2. Implement `reverse_resolve` with canonicalization + longest-match semantics.
3. Wire it into the local-start path; emit the Phase-B warning when no root
   matches.
4. Tests: inside-root cwd, nested roots, symlinked cwd, outside-roots cwd
   (warn, still works), root mapping changed after session start.
5. `just sandboxed-check`; host/macOS path checks recorded as NOT RUN.

## 5. Acceptance

- [ ] A compat `start` whose cwd sits inside a named root records the logical
      root-relative identity.
- [ ] Nested/overlapping roots resolve deterministically (documented rule).
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
