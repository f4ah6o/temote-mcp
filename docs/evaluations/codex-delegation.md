# Codex delegation evaluation

Status: implementation and fake-transport verification complete; one real `codex delegate` dogfood task is recorded below; app-server live run and comparative measurement remain pending.

## Current evidence

- `codex delegate` uses `codex exec --ignore-user-config --ephemeral --sandbox workspace-write`, bounded JSONL/stderr capture, a schema-validated final report, and filtered usage fields.
- The app-server adapter uses local stdio, an exact `0.153.4` compatibility check, session-instance and canonical-scope ownership, durable pre-side-effect operation receipts, child approvals independent of Temote yolo, bounded evidence, and reconciliation states. Pre-thread transient failures are retryable with the same start operation; uncertain thread/turn boundaries remain reconciliation-required. Unexpired task records, including terminal records, are retained until task retention expires; only expired terminal records without a live runtime are prunable, and a full scope rejects new starts. Compacted operation receipts fail closed on exact replay for the full task retention period.
- Rust unit/property tests, gateway contract tests, formatting, clippy, no-default-features checks, and diff checks are the repeatable verification set for this implementation.

## Live dogfood record (2026-09-11, macOS)

Environment: Codex CLI `0.153.4` installed through Vite+ (`vp`), `codex` resolved from `~/.vite-plus/bin`, existing Codex authentication present. Command run from a disposable git repository with one base commit:

```sh
temote-mcp codex delegate \
  --model gpt-5.6-luna \
  --reasoning-effort high \
  --prompt 'Create hello.txt containing exactly: ok ...'
```

Result (non-secret fields):

- parent result `status=success`, child exit code `0`, `artifacts_truncated=false`;
- requested model/effort: `gpt-5.6-luna` / `high`;
- observed model/effort: `null` (the child event stream did not expose them; requested values were not copied into observed);
- evidence: `thread_id` present; usage fields captured as `input_tokens=50442`, `cached_input_tokens=32256`, `output_tokens=512`, `reasoning_output_tokens=151`;
- child report `status=completed`, summary `Created hello.txt containing ok.`, one changed file, evidence artifact paths bounded and private;
- worktree verification: `hello.txt` was created in the delegated repository and `README.md` was untouched;
- no credential values, prompts, transcripts, or raw logs are recorded here.

Limitations observed: the child's `changed_files` entry was an absolute artifact path rather than a repository-relative path, and the child-reported `requested_model`/`requested_effort` fields were empty; the parent result kept requested values distinct from observed values. This is child report content, not a Temote serialization defect.

## Remaining live work

The app-server path has not been exercised against a live Codex on this host, and no direct-Temote / `codex exec` / app-server comparison has been run. Run those with the same base commit, permissions, and acceptance criteria, then record observed model/effort, usage source, process outcome, elapsed times, retries, and parent intervention. Do not infer token or cost savings from MCP response bytes.
