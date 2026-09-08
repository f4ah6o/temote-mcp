# Codex delegation evaluation

Status: implementation and fake-transport verification complete; real Codex dogfood and comparative measurement remain pending.

## Current evidence

- `codex delegate` uses `codex exec --ignore-user-config --ephemeral --sandbox workspace-write`, bounded JSONL/stderr capture, a schema-validated final report, and filtered usage fields.
- The app-server adapter uses local stdio, an exact `0.153.4` compatibility check, session-instance and canonical-scope ownership, durable pre-side-effect operation receipts, local approvals, bounded evidence, and reconciliation states.
- Rust unit/property tests, gateway contract tests, formatting, clippy, no-default-features checks, and diff checks are the repeatable verification set for this implementation.

## Environment limitation

The selected `codex` executable was present, but its embedded macOS vendor binary was missing (`ENOENT`). Therefore this checkout does not claim a real Luna Max task, real app-server model listing, authentication success, or measured usage. No credential values, prompts, transcripts, or raw diagnostic artifacts are recorded here.

## Required follow-up

After installing a validated Codex build on the target host, run one small implementation task with the requested model/effort, review its report against the worktree and checks, then record observed model/effort, usage source, process outcome, elapsed times, retries, and parent intervention. Run the direct-Temote, `codex exec`, and app-server comparison only with the same base commit, permissions, and acceptance criteria; do not infer token or cost savings from MCP response bytes.
