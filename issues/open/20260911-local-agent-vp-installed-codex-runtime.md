# `local_agent_run` cannot launch Codex installed through Vite+ (`vp`)

Status: open / reproduction and classification landed on main; sandbox fix not implemented
Created: 2026-09-11
Priority: P1 developer workflow regression
Related:
- `src/local_agent.rs`
- `src/sandbox.rs`
- `src/sandbox/macos.rs`
- `src/sandbox/linux/`
- `issues/open/20260910-developer-execution-broker.md`
- `issues/open/20260911-default-agent-permission-mode.md`

## Observed behavior

Codex is installed/managed through Vite+ (`vp`).

From a normal Temote session in the current `ask` permission mode:

1. `local_agent_run` with `agent=codex` reaches the normal Temote approval flow.
2. The operator approves the request.
3. The Codex child still cannot be executed successfully.

This is therefore not merely the existing `ask` approval friction tracked by `20260911-default-agent-permission-mode.md`. Approval is granted and execution still fails.

A Codex installation that is directly available as a conventional standalone executable must continue to work. The fix must also support package-manager/runtime-managed installations such as Vite+ without weakening the local-agent sandbox.

## Current implementation relevant to the failure

`local_agent_run` currently:

- captures a bounded allow-listed `PATH`/environment;
- searches absolute `PATH` entries for the fixed executable name (`codex` or `opencode`);
- rejects executables inside session roots;
- keeps both the selected runtime path and canonical target and revalidates them after approval;
- constructs the child argv internally rather than accepting caller-controlled executable/argv;
- exposes the runtime executable parent and canonical target parent as read-only roots to the local-agent sandbox;
- replaces HOME/CODEX_HOME with private per-run state while importing only the supported authentication file.

Those properties must be preserved.

## Working hypothesis

A Vite+-managed `codex` command may not be a self-contained executable. It may be a shim, symlink, wrapper script, or launcher that depends at runtime on one or more paths that are not represented by only:

```text
selected PATH candidate parent
canonical executable target parent
```

Possible missing dependencies include, depending on the actual Vite+ installation layout:

- the Vite+ package store containing the real Codex package/runtime;
- an interpreter selected by a shebang;
- a sibling runtime/helper binary;
- package-manager-generated launcher metadata;
- an additional symlink/wrapper hop that canonicalizing the top-level command does not model;
- a runtime lookup path that exists on the host but is hidden/not re-exposed in the local-agent sandbox.

These are hypotheses only. Do not broaden sandbox visibility until the exact failing lookup/path is captured.

## Goal

Allow `local_agent_run(agent=codex)` to execute a valid Codex installation managed by Vite+ after normal authorization, while preserving all existing local-agent security invariants.

The solution should generalize to a small, explicit class of package-manager/wrapper-based installed agent executables rather than hard-coding one user's absolute Vite+ directory.

## Non-goals

- Do not make the whole session yolo.
- Do not expose arbitrary executable paths or raw argv through MCP.
- Do not add the entire package-manager home, user home, or `$PATH` tree as writable/visible by default.
- Do not copy arbitrary package-manager state into the private agent HOME.
- Do not disable executable revalidation across approval.
- Do not solve this by automatically invoking `vp` with caller-controlled arguments.
- Do not conflate this bug with changing `ask` -> `agent` approval policy.

## First implementation slice — reproduce and classify only

Before changing sandbox policy, add deterministic coverage for launcher layouts representative of the failure.

### Required fixtures

At minimum cover:

1. standalone executable:
   ```text
   bin/codex
   ```
2. symlinked executable:
   ```text
   bin/codex -> store/codex
   ```
3. wrapper/shebang launcher:
   ```text
   bin/codex -> or invokes runtime/interpreter outside bin/
   ```
4. package-store launcher where the selected `codex` needs a sibling or package-store path at runtime.

The fixture does not need Vite+ itself if the same filesystem/process shape can be reproduced deterministically. Separately capture the real `vp` installation shape on a development host without recording credentials or unrelated user paths.

### Evidence to capture

For the real failing installation, record only non-secret diagnostics:

- `PATH` entry from which `codex` is selected;
- selected runtime path;
- canonical target path;
- file type (regular file/symlink/script as applicable);
- shebang interpreter when present;
- bounded symlink/launcher dependency chain;
- exact failing operation class (`ENOENT`, permission denied, sandbox deny, missing interpreter/helper/module, etc.);
- which required path was not visible/executable in the sandbox.

Do not record Codex auth contents, tokens, prompts, or full unrelated environment.

**Done when:** there is a deterministic test that fails for the same reason as the operator's Vite+-installed Codex and the missing runtime dependency is identified.

## Fix direction

Choose the narrowest fix supported by the reproduction.

Potential acceptable approaches include:

### A. Verified launcher dependency closure

Resolve a bounded set of launcher dependencies before execution, for example:

- selected PATH candidate;
- canonical target;
- verified shebang interpreter;
- explicitly discovered package runtime/helper path required by that launcher.

Expose only the required parent/files as read-only sandbox inputs.

### B. Package-manager-aware installed executable resolution

If Vite+ exposes a stable, non-secret way to resolve the installed command/runtime, add an internal resolver that verifies that layout and returns a bounded runtime dependency set.

This must remain an implementation detail. MCP callers still select only `agent=codex`.

### C. Preserve wrapper runtime path rather than over-canonicalizing execution

If the bug is caused by executing/validating the wrong path representation, preserve the package-manager-generated runtime entry point while separately validating its canonical target and dependencies.

The existing post-approval executable identity check must remain fail-closed.

## Safety invariants

- Caller still cannot provide an executable path.
- Fixed agent names remain `codex` / `opencode` only.
- Executable resolution must remain outside session roots.
- Executable/launcher identity is revalidated after approval and before launch.
- Added runtime paths are read-only unless a separately reviewed state/cache path requires write access.
- Do not expose all of `$HOME`, a complete package store, or arbitrary PATH directories merely for convenience.
- Symlink changes between prepare/approval/run fail closed.
- Wrapper/interpreter discovery is bounded and cycle-safe.
- Environment inheritance remains default-deny.
- Existing auth isolation and protected metadata behavior remain unchanged.
- Public MCP still cannot supply raw argv, executable paths, `without_sandbox`, or yolo.

## Required tests

- [ ] standalone Codex executable still launches through `local_agent_run`.
- [ ] Vite+-shaped Codex launcher fixture launches successfully after the narrow fix.
- [ ] approval followed by executable/symlink/launcher target replacement is rejected.
- [ ] missing interpreter/helper/package runtime reports a precise deterministic error rather than generic `codex not found`.
- [ ] launcher dependency traversal is bounded and rejects cycles/unsafe paths.
- [ ] no added runtime dependency becomes writable unless explicitly required and reviewed.
- [ ] package-manager state outside the required dependency set remains hidden/unavailable.
- [ ] `read_only` and `workspace_write` local-agent access retain their existing workspace semantics.
- [ ] Codex auth/environment isolation tests remain green.
- [ ] OpenCode `local_agent_run` does not regress.
- [ ] macOS coverage reproduces the operator-reported installation class; Linux gets equivalent wrapper/symlink regression coverage where applicable.

## Acceptance criteria

- [ ] A Codex installation created/managed through Vite+ can be selected and launched by `local_agent_run(agent=codex)` after authorization.
- [ ] The fix is based on the verified launcher/runtime dependency shape, not an absolute user-specific path exception.
- [ ] No whole-session yolo or broad HOME/package-store exposure is required.
- [ ] Existing executable identity revalidation across approval remains fail-closed.
- [ ] Existing local-agent sandbox, bounded output/job ownership, auth isolation, and protected metadata tests remain green.
- [ ] The operator-facing error identifies the missing launcher/runtime dependency class when execution cannot be made safe.

## Recommended next action

Implement the **reproduction/classification slice only** first. Capture the real Vite+ `codex` launcher shape and add a deterministic failing fixture before changing `executable_read_only_roots` or sandbox policy.

## Reproduction and classification (2026-09-11, macOS)

Real operator installation shape, captured without credentials:

```text
PATH candidate:      /Users/<user>/.vite-plus/bin/codex
candidate type:      symlink -> ../current/bin/vp
intermediate:        /Users/<user>/.vite-plus/current -> 0.2.9
canonical target:    /Users/<user>/.vite-plus/0.2.9/bin/vp
target type:         Mach-O 64-bit executable arm64 (Vite+ multicall CLI, uses VP_HOME)
runtime dependency:  /Users/<user>/.vite-plus/packages/@openai/codex/<installId>/lib/node_modules/@openai/codex/bin/codex.js
package metadata:    /Users/<user>/.vite-plus/packages/@openai/codex.json
managed runtime:     /Users/<user>/.vite-plus/js_runtime
```

This matches the working hypothesis: the command is not a self-contained executable, and a package-manager runtime layer is resolved through `VP_HOME` after launch.

Classification with deterministic fixtures (`src/local_agent.rs` tests):

- `vite_plus_shaped_launcher_resolves_through_current_symlink` resolves the two-hop symlink chain; the current resolver handles the candidate/target pair.
- `vite_plus_shaped_launcher_exposes_only_bin_roots_today` shows that `executable_read_only_roots` exposes only the candidate parent (`.../bin`) and the canonical target parent (`.../0.2.9/bin`). The intermediate `current` symlink parent (`.../.vite-plus`) and the package store remain outside the visibility closure.
- `vite_plus_launcher_cannot_read_package_store_in_local_agent_sandbox` (ignored) reproduces the operator failure class: the sandboxed launcher cannot exec/read through the missing dependency path (`sandbox-exec: execvp() ... Operation not permitted`).

Identified missing runtime dependency class: launcher-side paths needed to resolve and run the package runtime, at minimum the intermediate symlink hop parent, the Vite+ package store entry for the agent package, and the managed runtime directory. None of these should become globally writable; the fix should add a bounded, verified read-only dependency closure (fix direction A/B/C in this issue).
