# S0a: typed session source contract

Status: initial implementation merged (PR #86) / conformance to the strengthened contract pending — tracked by `issues/polished/20260929-s0a-contract-conformance.md`

Repository: `f4ah6o/temote-mcp`  
Parent: `issues/open/20260929-session-first-managed-provisioning.md`  
Created: 2026-09-29 (Asia/Tokyo)  
Observed baseline: `main` `3947fc2a6d1e53e0bc85219a176ab4240b5abc16`

Revision note: this packet's contract was strengthened after the PR #85 design review. The S0a implementation merged in PR #86 predates the inherited component grammar and the checked-entry-point requirements below; conformance is tracked by `issues/polished/20260929-s0a-contract-conformance.md` and must land before this type is wired into S1+ provisioning.

## 1. Goal

Introduce the typed seam needed for session-first provisioning without changing current runtime behavior or the public MCP `session_start(path=...)` wire contract.

The codebase currently assumes a path-first start in several places:

- `src/session_control.rs`: `ControlRequest::Start { path, session_id, ... }`
- `src/supervisor.rs`: `start*_with_environment(logical_path, ...)`
- `src/config.rs`: persisted `Session { cwd, ... }`
- `src/session_control.rs`: `SessionView.workspace` derives a managed-worktree view from cwd
- `src/mcp.rs`: server instructions and public `session_start` teach configured named-root path

This packet does **not** replace those paths yet. It creates a backend-neutral typed input model that later provisioning packets can wire into the supervisor.

## 2. Scope

Landed: the initial module (`src/session_source.rs`) was added in PR #86. The scope below is the original packet text, kept as history; the remaining conformance delta is listed in §13.

Add one small module, preferably `src/session_source.rs` (or an equivalent orchestration-core location if the implementation finds an already suitable module), containing:

```rust
struct RepositoryId {
    host: String,
    owner: String,
    name: String,
}

enum VcsPreference {
    Auto,
    Jujutsu,
    Git,
}

enum SessionStartSpec {
    ManagedRepository {
        repository: RepositoryId,
        base: Option<String>,
        vcs: VcsPreference,
    },
    ExistingWorkspace {
        logical_path: String,
    },
}
```

Names may vary if an existing code convention requires it, but the semantic split must remain explicit.

## 3. Repository source normalization

Provide a typed parser/normalizer for repository identity.

Required accepted input:

```text
f4ah6o/temote-mcp
github.com/f4ah6o/temote-mcp
https://github.com/f4ah6o/temote-mcp
https://github.com/f4ah6o/temote-mcp.git
git@github.com:f4ah6o/temote-mcp.git
```

Rules:

- `owner/repo` uses an explicit configured/default host argument supplied to the parser; do not read ambient Git config or cwd.
- strip one terminal `.git` suffix from repository name.
- reject empty host/owner/name.
- reject absolute filesystem paths.
- reject `..`, path traversal, query/fragment components, credentials other than the SSH `git@` syntax above, and extra path components.
- normalize host case.
- for `github.com`, normalize owner/repository case consistently for identity comparison.
- do not contact the network.
- do not infer owner/name from a local checkout.

Component grammar (inherited from F1 §2.2, `issues/done/20260925-f1-repository-store-workspace-contract.md`):

- every `host` / `owner` / `name` component must match `^[A-Za-z0-9][A-Za-z0-9._-]*$`. Do not widen or relax the F1 store grammar in this packet.
- the supplied default host argument is validated by the same grammar.
- validate the name again after the terminal `.git` strip: a name that becomes empty or invalid once stripped is rejected.
- check the raw input's forbidden structures before or while splitting components — `..` / dot segments, encoded separators such as `%2F` / `%5C` / `%2f`, whitespace, and control characters must be rejected even if a URL parser or normalization step would fold them away later.
- normalization is limited to the forms defined above: host case, `github.com` owner/name case, and one terminal `.git` strip. Do not implicitly repair invalid input — no whitespace trimming to accept, no backslash-to-slash conversion, no percent-decoding into a different identity.
- dot / dot-dot component rejection and separator rejection remain in force alongside the grammar.

Checked entry points:

- `RepositoryId` may only be produced through the validating parser (or an equivalent checked constructor). Do not add a struct-literal, `Deserialize`, or other entry path that can materialize an identity violating the §3 rules.
- serde input must enforce the same invariants as parser input: malformed serialized `RepositoryId` data fails closed, and a serde round-trip of a normalized identity preserves value and identity.
- this is a contract hardening for later packets; it does not assert that an unimplemented `Deserialize` path has a concrete bug today.

The parser should return a typed validation error rather than an arbitrary shell/Git error.

## 4. Existing workspace semantics

`SessionStartSpec::ExistingWorkspace` represents the current path-based compatibility model.

In this packet:

- retain the logical named-root-relative string as input
- do not accept a new absolute-path remote form
- do not reverse-resolve cwd here (NR1 owns that compatibility behavior)
- do not change `ControlRequest::Start`
- do not change persisted `Session`

The purpose is to prevent future core code from treating path-based and repository-managed start as the same untyped string.

## 5. Managed repository semantics

`ManagedRepository` contains no physical workspace/store path and no delivery branch.

Allowed:

- RepositoryId
- optional requested base ref/name
- explicit VCS preference

Not allowed in the type:

- `PathBuf`
- arbitrary store root
- workspace path
- Git branch required for start
- Git worktree name
- raw Git/jj argv

A later packet resolves these through host policy and allocates WorkspaceId.

## 6. VCS preference

Contract:

```text
auto      -> prefer Jujutsu; capability result decides whether caller must choose Git
jujutsu   -> require Jujutsu; fail explicitly if unsupported
git       -> explicit compatibility backend
```

This packet only models the preference. It does not run capability detection or fallback.

No silent `auto -> git` implementation should be introduced here.

## 7. Read/change scope

Landed in PR #86; kept as the original change-scope record.

Expected:

- new `src/session_source.rs` or equivalent
- crate/module declaration in `src/main.rs` and/or `src/lib.rs` as required
- unit tests colocated with the new module

Avoid unless compilation requires a minimal visibility change:

- `src/session_control.rs`
- `src/supervisor.rs`
- `src/mcp.rs`
- `src/config.rs`
- `src/vcs.rs`

Do not change MCP tool count or schema in this packet.

## 8. Tests

Unit tests must cover at least:

1. `owner/repo` + supplied default host
2. explicit `host/owner/repo`
3. HTTPS source
4. HTTPS `.git` suffix
5. SSH `git@host:owner/repo.git`
6. GitHub identity case normalization
7. malformed one-component repository
8. too many path components
9. `..` traversal
10. absolute filesystem path
11. HTTPS query / fragment rejection
12. credential-bearing HTTPS URL rejection
13. ManagedRepository round-trip serde
14. ExistingWorkspace round-trip serde
15. ManagedRepository serialized form contains no path/store/branch field
16. percent-encoded separators rejected: `github.com/f4ah6o/repo%2Fother`, `github.com/f4ah6o/repo%5Cother`, `github.com/f4ah6o/repo%20name`
17. literal backslash inside `owner`/`name` rejected
18. leading / trailing / internal whitespace rejected
19. tab / newline / NUL / other control characters rejected
20. dot (`.`) and dot-dot (`..`) path components rejected — including inside URL forms and combined with encoded separators
21. default host argument containing invalid characters or separators rejected
22. name that becomes empty after the `.git` strip rejected
23. malformed `RepositoryId` serde input rejected (an unchecked deserialization path must not materialize an invalid identity)
24. property: allowed notation variants of the same repository normalize to the same `RepositoryId`
25. property: every component of a successfully parsed `RepositoryId` satisfies the F1 grammar
26. property: serde round-trip of a normalized `RepositoryId` preserves value and identity
27. property: malformed input is never silently converted into a plausible different identity

## 9. Validation

Run:

```sh
cargo fmt --all -- --check
cargo test --bin temote-mcp --all-features --locked session_source
cargo test --bin temote-mcp --all-features --locked
cargo clippy --all-targets --all-features -- -D warnings
cargo check --no-default-features --all-targets
```

If project policy requires `just sandboxed-check`, run it and report separately.

Do not report unexecuted host/macOS gates as PASS.

## 10. Non-goals

- public repository-first `session_start`
- RepositoryStore ensure/fetch
- workspace allocation
- jj workspace creation
- session metadata schema migration
- lifecycle state changes
- delivery bookmark/ref
- deprecating existing path start
- changing NR1 behavior

## 11. Acceptance

- [ ] core has a typed distinction between ManagedRepository and ExistingWorkspace
- [ ] RepositoryId is independent of cwd/path
- [ ] accepted repository spellings normalize deterministically
- [ ] every host/owner/name component satisfies the inherited F1 grammar `^[A-Za-z0-9][A-Za-z0-9._-]*$`
- [ ] malformed/path-like inputs fail closed
- [ ] no unchecked entry point (constructor, serde) can produce a `RepositoryId` that violates §3
- [ ] invalid input is rejected, never implicitly repaired into a different identity
- [ ] ManagedRepository cannot carry physical workspace/store path or required delivery branch
- [ ] no public session behavior changes
- [ ] existing tests stay green

## 12. Follow-on

Before the follow-on packets wire `SessionStartSpec` / `RepositoryId` into provisioning, the conformance fixes tracked by `issues/polished/20260929-s0a-contract-conformance.md` must land (§13).

After S0a:

1. S1a RepositoryStore adapter
2. S2a named-root-backed workspace allocator
3. S2b bare store -> jj workspace / logical change
4. S3a repository-first session_start composition
5. S4a verified revision -> delivery ref

## 13. Remaining conformance work

Tracked by `issues/polished/20260929-s0a-contract-conformance.md` (full acceptance criteria there). At `main` `d3b7d53`, static inspection of `src/session_source.rs` shows:

- `validate_component()` enumerates forbidden characters instead of enforcing the F1 grammar `^[A-Za-z0-9][A-Za-z0-9._-]*$` — e.g. `repo%2Fother` parses because `%` is not on the deny-list.
- `RepositoryId` derives `Deserialize` and exposes public fields, so serde input and struct literals bypass the parser's validation/normalization (`deny_unknown_fields` constrains field names only).

Remaining work: component grammar for `host` / `owner` / `name` and the supplied default host; checked constructor / parser / serde enforcing identical invariants; rejection of percent escapes, backslashes, whitespace, NUL/control characters and malformed serde input; the §8 negative and property tests (16–27); serde round-trip preserving a normalized identity; GitHub case normalization kept. These fixes and their verification must land before S1+ provisioning wires this type in.
