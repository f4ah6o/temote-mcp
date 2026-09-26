# AGENTS.md

## Purpose

Temote MCP is a Rust MCP server for delegating local-machine work through explicit sessions. Temote does not execute files, commands, Git, or host integrations directly: machine operations run inside a coding agent on the local machine, driven through the delegation backends (Codex app-server, `opencode serve` via the OpenCode SDK, Devin ACP, and the Devin Cloud API). Sessions remain path-scoped; the `ask` permission mode keeps the local approval console for host/network-sensitive operations, the default `agent` mode is approval-free for validated structured operations, and `--yolo` intentionally removes those Temote MCP boundaries.

## Repository rules

- Product name: **Temote MCP**.
- CLI/package name: `temote-mcp`.
- Environment-variable prefix: `TEMOTE_MCP_`.
- Keep `README.md` and `README.ja.md` short and user-oriented: what it is, installation, first session, Agent Skill installation, and links to deeper docs.
- Put detailed human documentation under `docs/`.
- Put repository-specific agent/development guidance here instead of expanding the README.
- Do not add product-specific web-chat setup instructions. Document standards-based MCP/OAuth behavior in client-neutral terms.
- Preserve upstream attribution to `nakasyou/local-mcp` and the Temote naming credit.

## Safety invariants

Do not weaken these without an explicit issue describing the security model change:

- Session-bound task, evidence, and job tools require `session_id`. Federated discovery/lifecycle tools are documented exceptions: `host_list`, `host_info`, host-aware `session_list`, and `session_start` may be sessionless only for host discovery or lifecycle scope.
- Authenticated direct-HTTP `upgrade_preflight` and `upgrade_status` are sessionless host-lifecycle exceptions. `upgrade_apply` requires an active managed normal session and explicit local-user approval. These tools must not be exposed by stdio MCP or the gateway.
- Remote `session_start` is host-scoped and limited to named-root-relative normal sandbox sessions; it must not create `--yolo` sessions.
- Unqualified session routing must fail closed whenever ownership is ambiguous or cannot be determined because any relevant leased host/session status is unavailable. Explicit `host_id` routing must remain isolated from unrelated host discovery failures.
- `host_id` is a non-secret routing identity, not a credential; authenticated host identity must stay bound to configured credentials and generation state.
- Public tools must not inherit unrestricted local `--yolo` semantics merely because a local host/session uses yolo mode.
- Delegation tools expose typed task contracts only: a caller can supply a task, model/effort/agent selectors, and typed `steer`/`resume`/`interrupt` control actions — never an executable, raw argv, environment block, network policy, or a path outside the session's canonical scope. `operation_id` is mandatory so accepted side effects stay idempotent and reconcilable.
- Detailed task transcripts/output cross the boundary only as bounded, expiring, session-owned evidence records read through `evidence_read`; tool responses must not inline unbounded child output.
- Permission policy is centralized on operation class x `PermissionMode`. `ask` keeps approval-gated host/network/structured operations. The default `agent` mode removes only the Temote-local approval prompt for otherwise-valid structured operations (delegated task start/control) and must never widen sandbox, path, network, or tool-specific capability.
- New local managed and authenticated public sessions default to `agent`; public HTTP must not create or promote `yolo`.
- `--yolo` may bypass Temote MCP sandbox/path/approval boundaries, but should not silently change unrelated client authorization semantics.
- Secrets must not be written to session metadata, audit logs, approval summaries, or ordinary tool output.
- Child MCP approval summaries should expose argument keys, not secret values.

## Tool behavior that agents should preserve

- Machine work is delegated, not executed: `*_status` probes a backend once, `*_task_start` accepts an idempotent task (fresh UUID `operation_id`), `*_task_get` reads/reconciles retained tasks, `*_task_control` applies typed steer/resume/interrupt actions.
- Work that outlives the foreground timeout returns a session-owned `job_id` for `poll_job` / `job_list` / `stop_job`; background jobs are cancelled when the session stops or reaches its lifetime limit.
- Delegated task detail is exposed only through bounded scoped evidence; read it with `evidence_read`.

## Development workflow

Before changing code, inspect the current worktree and relevant issue/document instead of assuming prior state. Keep changes generic rather than adding product/project-specific exceptions.

Run the relevant checks before committing:

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
(cd gateway && npm test)
git diff --check
```

`just check` covers the normal Rust format/test/clippy/diff gates. Run gateway tests when gateway code or shared protocol behavior changes.

When Temote MCP itself is being developed from inside an already-sandboxed normal Temote session, use `just sandboxed-check` for the deterministic repository-local subset. Treat every host-only line it prints as `NOT RUN`, not PASS. Nested Linux bubblewrap/userns acceptance, session-GC Unix-socket liveness (`session_control::tests::host_liveness_tests`), local-agent real-wiring, gateway deployment-preflight CLI subprocess tests, local Unix-socket integration, and process-boundary E2E remain host/CI gates; do not weaken the current session sandbox or mark those tests successful merely because the outer sandbox prevents them from starting. `just linux-sandbox-acceptance` is the explicit Linux host gate when running on a suitable unsandboxed development host.

### Mutation and property testing

The suite pairs [`noprop`](https://github.com/sile/noprop) property tests (see `docs/development.md` "Property-based tests") with [cargo-mutants](https://mutants.rs) mutation testing: noprop checks invariants over generated inputs, cargo-mutants checks whether any test notices when the implementation changes.

- Install once with `cargo install --locked cargo-mutants`; shared settings live in `.cargo/mutants.toml`.
- Always scope runs — a full-tree run is a long host/CI job. Use `just mutants -f src/<file>.rs` (or `-F`/`-E` regexes) for the file you changed and `just mutants-diff` for mutants touched by the current diff against `origin/main`. Preview the mutant list cheaply with `just mutants-list`. Re-running keeps prior verdicts via `--iterate`.
- The baseline must be green: cargo-mutants runs the unmutated suite first. On machines where tests unrelated to your change are already failing (e.g. network-dependent probes), append `-- --skip <test-filter>` to exclude them, and prefer the sandboxed-safe subset from `just sandboxed-check`.
- Read each `MISSED`/`unviable` survivor instead of chasing a kill percentage. Kill survivors with one reference-model or round-trip noprop property via `test_support::run` rather than many example tests — keep boundary values like length 0 in the generated space or guard branches escape the property. A mutant that survives because the change is provably equivalent (e.g. `>` vs `>=` where `==` gives the same result) is expected; document it in review rather than contorting tests.
- Replay a property failure with `TEMOTE_PBT_SEED=<seed>` (decimal or hex `u64`).

## Documentation map

- `docs/usage.md` / `docs/usage.ja.md`: sessions, permissions, delegation backends, tool behavior, safety boundaries.
- `docs/public-http.md` / `docs/public-http.ja.md`: Cloudflare Access/Tunnel public HTTP deployment.
- `docs/gateway.md` / `docs/gateway.ja.md`: Workers/Durable Objects multi-host gateway.
- `docs/development.md`: build, test, release, and contributor details.
- `skills/temote-mcp/SKILL.md`: reusable Agent Skill for operating Temote MCP from a compatible agent.

When behavior changes, update the narrowest relevant document and the skill only if agent-operating guidance also changed.

## Release

Releases use CalVer `YYYY.MM.PATCH` in `Asia/Tokyo` through `f4ah6o/calver-action`. The `latest` tag selects the release candidate; the allocator workflow creates a release-only version commit and immutable CalVer tag rather than merging that version bump back into `main`, then dispatches cargo-dist on that tag. Keep binary distribution settings in `dist-workspace.toml` and regenerate `.github/workflows/release.yml` with `dist generate` instead of hand-editing the generated workflow.

- Version bumps are owned exclusively by the GitHub Actions CalVer workflow. Do not manually edit `Cargo.toml`, `Cargo.lock`, or other package/version metadata merely to advance the Temote version on `main` or in an implementation branch.
- A local rebuild/install from `main` may therefore report the repository's baseline package version and must not be treated as a release-version bump. Release/version verification should use the CalVer workflow output/tag rather than locally mutating version metadata.
