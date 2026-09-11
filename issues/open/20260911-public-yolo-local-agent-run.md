# `local_agent_run` — locally started yolo sessions are rejected by the public MCP endpoint

## Status

Bug / contract inconsistency. Reproduced on 2026-09-11.

## Problem

A Temote session can be started locally with `permission_mode=yolo` and remain active, but a remote client connected through the public MCP endpoint cannot invoke `local_agent_run` against that session.

Observed error:

```text
yolo sessions are unavailable on the public MCP endpoint
```

This blocks the workflow where a user deliberately starts a local yolo session and then delegates implementation work to a local Codex/OpenCode agent through `local_agent_run`.

The problem is specifically the public MCP routing boundary. The yolo session itself remains active.

## Reproduction

1. Start a Temote session locally in yolo mode.
2. Confirm `session_list` reports it as active with `permission_mode=yolo` and `yolo=true`.
3. From a client connected through the public MCP endpoint, call:

   ```text
   local_agent_run
     session_id: <active-yolo-session>
     agent: codex
     task: <bounded task>
     access: read_only | workspace_write
   ```

4. The call fails before the local-agent-specific execution/approval path with:

   ```text
   yolo sessions are unavailable on the public MCP endpoint
   ```

A read-only reproduction probe on 2026-09-11 produced exactly this error.

## Current implementation

`src/mcp.rs` applies a blanket public/yolo rejection immediately after loading the session and before dispatching on the tool name:

```rust
let session = config::load_session(&session_id).await?;
anyhow::ensure!(
    !public || !session.yolo,
    "yolo sessions are unavailable on the public MCP endpoint"
);
match name {
    // ...
    "local_agent_run" => local_agent_run(&args, &session, local_agent_executable).await,
    // ...
}
```

Therefore the public endpoint rejects every session-scoped tool for a yolo session before `local_agent_run` can apply its own security boundary.

The HTTP tests currently assert this blanket rejection.

## Contract inconsistency

The structured local-agent broker has a stronger, tool-specific boundary than ordinary yolo execution:

- `local_agent_run` accepts only a bounded task and `read_only` / `workspace_write` access mode.
- The caller cannot provide an executable, raw argv, environment, or network policy.
- The selected `cwd` is canonicalized and remains within the session roots, including for yolo sessions.
- Agent state is isolated and inherited environment is minimized.
- The broker deliberately retains its own explicit local approval boundary even for yolo sessions.

The current documentation and Agent Skill already describe this yolo-specific local-agent behavior, while the public MCP dispatcher prevents that path from ever being reached.

The public gateway also intentionally exposes `local_agent_run` as a routed tool.

## Desired behavior

Allow `local_agent_run` to target a **locally created, already-active yolo session** through the public MCP endpoint without making yolo generally available remotely.

This must remain a narrow tool-specific exception, not a relaxation of the public HTTP security model.

The following invariants remain unchanged:

- Public clients cannot create a yolo session.
- Public clients cannot promote a normal session to yolo.
- Public HTTP does not expose `without_sandbox`.
- Ordinary public calls do not gain generic access to yolo sessions merely because this exception exists.
- `local_agent_run` retains its explicit child approval boundary for yolo sessions.
- Existing workspace canonicalization, protected metadata, environment minimization, bounded output, cancellation, and secret-isolation rules remain intact.

## Recommended design

Replace the current global `public && session.yolo` rejection with an explicit per-tool public/yolo policy.

Conceptually:

```text
PublicYoloPolicy
  deny                    # default for session-scoped tools
  allow_with_own_boundary # e.g. local_agent_run
```

`local_agent_run` should be classified as `allow_with_own_boundary` because it already provides a separate reviewed execution and approval contract.

Do not implement this as a general allow-list bypass hidden inside unrelated session loading logic. Keep the policy visible and testable at the MCP dispatch boundary.

## Required tests

- [ ] A public `local_agent_run` call against a locally created active yolo session reaches the local-agent approval path instead of failing at generic MCP dispatch.
- [ ] Denying that local-agent approval starts no child process/job.
- [ ] Approving it can execute the existing bounded Codex/OpenCode adapter contract.
- [ ] A public ordinary execution call against the same yolo session is still rejected.
- [ ] Public session creation still cannot request or produce yolo mode.
- [ ] Public HTTP still exposes no `without_sandbox` path.
- [ ] Normal-session `local_agent_run` behavior remains unchanged.
- [ ] Direct/local MCP behavior remains unchanged.
- [ ] Gateway/public tool contract tests remain green.
- [ ] Existing sandbox, approval, credential-isolation, Git metadata, and session-lifecycle regressions remain green.

## Acceptance criteria

- [ ] The workflow `locally start yolo session -> remotely call local_agent_run -> explicit local-agent approval -> bounded local agent execution` works.
- [ ] The public endpoint does not gain generic access to yolo session operations.
- [ ] The implementation uses an explicit per-tool policy rather than weakening the global security model implicitly.
- [ ] Documentation and `skills/temote-mcp/SKILL.md` describe the resulting public/yolo behavior consistently.
- [ ] Relevant Rust, HTTP, gateway, and live acceptance tests pass.
