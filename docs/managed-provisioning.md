# Managed repository provisioning

The authenticated `session_start` tool accepts exactly one of `path` or `source`. `path` keeps the existing named-root workspace behavior. `source` provisions a new, generated workspace and requires a caller-generated UUID `operation_id`:

```json
{"source":{"kind":"repository","repository":"f4ah6o/temote-mcp","base":"main","vcs":"auto"},"operation_id":"d850ca8e-3ec7-43ef-8b75-e8c889f9e126"}
```

`source` can also be a repository string, which selects `auto` and `main`. Set `TEMOTE_ROOTS` on the supervisor. When more than one named root exists, set `TEMOTE_WORKSPACE_ROOT` to the selected root name. `TEMOTE_MCP_ROOTS` and `TEMOTE_MCP_WORKSPACE_ROOT` remain read-only compatibility aliases; the canonical names take priority. `TEMOTE_SOCKET_NAMESPACE` similarly takes priority over `TEMOTE_MCP_SOCKET_NAMESPACE` for isolated local sockets.

The first accepted request stores its repository identity, selected root, generated SessionId, WorkspaceId, ChangeId, and delegated task operation ID before preparation. Retry with the same `operation_id` and identical request to inspect progress. A changed request conflicts. Keep the original operation ID after a lost response. A pending result is not an active session. The repository and environment tasks may need later exact retries to observe their terminal states. The environment task retains a separate operation UUID, model, effort, and input digest before delegation.

Automatic preparation uses the visible default advertised by Codex and its advertised default reasoning effort. A legacy inventory without default metadata keeps catalog order; hidden models are excluded and ambiguous defaults fail closed. Model and effort are saved before dispatch, so a receipt retry retains the accepted selection. Required HTTPS fetch still follows the child network approval path; a blocked report or missing ready marker does not activate a workspace.

The host places a bare Git store and an isolated workspace under the selected named root. The delegated Codex agent performs repository fetch and workspace creation within a temporary root-scoped normal session. A bounded allocation marker binds the operation, repository, workspace, change, pinned commit, and backend. For recognized Cargo or pnpm manifests, the same root-scoped agent prepares isolated dependencies and writes a separate environment ready marker. Temote validates both markers and the full preparation owner before activating the generated workspace session. An unsupported or ambiguous manifest set reports `environment_unsupported` and does not activate an agent-ready session. It does not create a local `main` checkout or alter existing checkouts.

`auto` and `jujutsu` require `jj`; absent `jj` is unsupported and never selects Git implicitly. `git` explicitly selects compatibility mode. Its workspace must contain an independent real `.git` directory inside the allocated workspace, without a linked common directory. No caller-supplied host path, argv, environment, or network policy is accepted.

The receipt records the accepted canonical root and full activated session instance, including scope and permission mode. A changed root, stopped instance, or replaced instance is never adopted on replay. Stopped stays stopped; an unprovable instance reports `unknown` or `reconciliation_required`. If the response is lost between Codex task acceptance and receipt update, Temote uses the same task operation UUID, prompt, model, and effort to read the retained Codex receipt and recover its task ID. A missing or uncertain backend receipt stays in reconciliation; Temote does not issue a new blind preparation start.
