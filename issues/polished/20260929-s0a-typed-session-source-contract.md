# S0a: typed session source contract

Status: ready for implementation  
Repository: `f4ah6o/temote-mcp`  
Parent: `issues/open/20260929-session-first-managed-provisioning.md`  
Created: 2026-09-29 (Asia/Tokyo)  
Observed baseline: `main` `3947fc2a6d1e53e0bc85219a176ab4240b5abc16`

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
- [ ] malformed/path-like inputs fail closed
- [ ] ManagedRepository cannot carry physical workspace/store path or required delivery branch
- [ ] no public session behavior changes
- [ ] existing tests stay green

## 12. Follow-on

After S0a:

1. S1a RepositoryStore adapter
2. S2a named-root-backed workspace allocator
3. S2b bare store -> jj workspace / logical change
4. S3a repository-first session_start composition
5. S4a verified revision -> delivery ref
