# Managed workspace environment preparation

An allocated workspace is not agent ready until its dependencies and toolchain
have been checked. The environment layer uses a versioned `PreparationPlan`
bound to a full session instance (ID, start time, process ID, permission mode,
canonical scope, permitted directories and grants), workspace UUID, operation UUID,
validated repository identity, workspace path, adapter, and input digest. A new
operation UUID is required for a new attempt. Reconciliation of an uncertain
attempt uses its original UUID and retained delegated task.

The `vp_pnpm` adapter requires `package.json` and `pnpm-lock.yaml`.
`node_modules` stays in the workspace. The
`cargo_sccache` adapter requires `Cargo.toml` and `Cargo.lock`. Cargo sources
and sccache storage may use the repository cache, while `CARGO_TARGET_DIR`
belongs to the workspace UUID. A workspace-scoped session keeps all caches
inside its workspace; a separately authorized root-scoped preparation session
can use a repository-specific shared cache. If sccache is absent, the delegated agent may
run Cargo without it and records a null sccache version. Neither adapter
accepts caller-supplied command arguments or environment variables.

The adapter generates a task for an existing normal coding-agent session. The
existing orchestration task start path retains its `ask` or
`agent` approval policy and backend sandbox. The agent checks installed tool
versions and supported flags, performs the setup, then atomically writes a
bounded marker. The host validates the marker's identity, scope, input digest,
versions, and ready status against the durable managed owner receipt and live
session instance. It also checks bounded `owner.json` records in the private
workspace cache and repository-specific shared cache before accepting ready.
The private owner binds the WorkspaceId and RepositoryId; the shared owner
binds the RepositoryId. A cache hit passes the same checks and produces the
same ready stamp as a cold setup. Failed, uncertain, missing, or stale markers
never mean ready.

Managed `session_start(source=...)` detects a supported adapter after verifying
the allocated repository marker and pinned base. It prepares through the
original root-scoped normal coding-agent session, storing the plan, input
digest, full owner and distinct operation UUID before delegation. It activates
the workspace session only when the allocation marker still pins the same base
and the environment marker validates against the plan. A lost task ID is
recovered from the original retained operation with its saved model and effort;
an unavailable receipt stays in reconciliation. A terminal failure reports
`environment_retryable`, and the next exact managed request uses a new attempt
UUID. Repositories without exactly one recognized manifest pair report
`environment_unsupported`; no arbitrary install script is inferred. Existing
`session_start(path=...)` behavior is unchanged.

`temote env-prepare start --session-id ID --operation-id UUID --adapter vp-pnpm
--model MODEL --effort EFFORT` (or `cargo-sccache`) admits a managed workspace
operation. `temote env-prepare status --session-id ID --operation-id UUID`
reconciles its retained task and marker. A failed attempt is retryable with a
new operation UUID; an uncertain start requires reconciliation of the original
UUID. The command checks the durable managed owner and persists its own
attempt receipt before delegation. It is available for explicit preparation of
an already active managed workspace; automatic managed activation uses its own
root-session attempt receipt. No host tool-version or cache benchmark result
is claimed by the deterministic module tests.
