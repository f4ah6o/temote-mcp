# OpenCode scoped workspace checks (OC2)

An OpenCode task may request `workspace_requirement: "managed_commands"` only when the Host has a ready managed workspace bound to the active session's canonical checkout. The Host must also set both `TEMOTE_OPENCODE_WORKSPACE_CODEX_MODEL` and `TEMOTE_OPENCODE_WORKSPACE_CODEX_EFFORT` to a Codex model and effort advertised by its local capability probe. This is an explicit Host opt-in to an additional coding-agent backend. Ordinary OpenCode tasks keep their existing behavior.

## Private tool contract

The installed OpenCode `v2.0.11` passed the private remote MCP registration gate. Temote uses the [V2 configuration](https://opencode.ai/v2/docs/mcp-servers): one server under `mcp.servers`, `disabled: false`, `codemode: false`, a loopback URL, runtime-only bearer header, OAuth disabled, and typed startup/catalog/execution timeouts. The [V2 MCP status route](https://dev.opencode.ai/v2/docs/api/mcp/v2-mcp-list/) returns a location envelope with a server array. Temote requires both the exact server's connected status and an authenticated `tools/list` observed by its own private bridge. It does not infer MCP registration from the builtin tool inventory. The bridge advertises only `workspace_check`, with fixed `inspect|build|test|lint|status` actions and a UUID `operation_id`; status also names its original action as `target`. Commands, argv, executables, environment, paths, network policy and task text are rejected.

Each action maps to fixed Temote-owned task text. The bridge invokes Codex through centralized orchestration with a private actor, so `ask` keeps Temote's approval and the instruction/outcome observation journal. A Host-only Codex origin binds the receipt fingerprint to the parent OpenCode TaskId, WorkspaceId, action, and parent execution generation. The parent retains a bounded list of exact child receipts before dispatch. Reads use the ordinary orchestrated TaskGet path after matching the origin. Codex applies its normal sandbox and stores detailed output in session-owned evidence. The MCP response projects only task ID, state, revision, operation ID, action, and an evidence reference. It does not forward OpenCode tool arguments into Codex task text or expose Temote's public MCP catalog.

The bridge holds a random bearer token in process memory and in the isolated child configuration, binds only to loopback, and ends with the per-task child. Every call rechecks the active full session instance, parent OpenCode TaskId, running status, execution generation and runtime instance, WorkspaceId, provisioning owner and marker, canonical checkout, and the stored Host model/effort choice. A stale bridge fails after parent steer, resume, or runtime replacement. The parent watcher interrupts only matching nonterminal retained Codex children when the parent terminates or its generation changes; session shutdown also attempts scoped cleanup, while Codex's session lifecycle remains the final cancellation boundary. The private child configuration directory is isolated from the user's global OpenCode plugins. A checkout with project OpenCode configuration or plugins is rejected; child edit/read permissions deny protected metadata and private runtime state. OpenCode's native shell remains denied.

Before task acceptance, Temote probes Codex availability and the selected model/effort, then launches an isolated OpenCode v2 child to verify the private MCP connection and authenticated catalog read. Failure returns `OPENCODE_PREFLIGHT_BLOCKER` with `execution_unavailable` and the missing capability. Startup performs the same registration check again and fails closed if runtime state changed.

## Verification status

The initial sandboxed fixture could not bind loopback (`EPERM`); automatic approval review rejected its escalation. The unsandboxed host test subsequently ran against installed OpenCode `2.0.11` and passed. It caught and fixed first-start private-directory initialization, the V2 config/status shape, and the connection/catalog startup race. This is transport registration proof. Provider-backed managed workspace `inspect`, build/test/lint, protected-state and escape acceptance remains **NOT RUN** until an authorized managed workspace and provider canary are available. Native shell remains denied.

### Unsandboxed installed-v2 registration canary

Run the repository gate with the installed executable's absolute path:

```sh
TEMOTE_TEST_OPENCODE_BINARY=/absolute/path/to/opencode cargo test --bin temote --locked installed_v2_registers_private_workspace_bridge_host_acceptance -- --ignored
```

The gate creates isolated private state and a random loopback bridge, checks an actual authenticated catalog read, then shuts down both sides. It performs no provider turn. Follow it with a real managed-workspace task using advertised Host opt-in Codex model/effort, each fixed action and same-key status, and failure cases for changed action, parent generation, protected metadata and path escape. Keep those provider results separate from the registration PASS.
