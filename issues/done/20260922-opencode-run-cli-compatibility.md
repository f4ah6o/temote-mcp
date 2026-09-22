# OpenCode run CLI compatibility

Status: doing

## Observed

On 2026-09-22, the actual broker failed before task creation because the installed OpenCode CLI reported:

- `Unrecognized flag: --pure`
- `Unrecognized flag: --dir`

## Implementation

Update the OpenCode invocation to use the verified installed CLI contract:

`run --standalone --format json`

The outer canonical working directory remains owned by `LocalAgentScope`; it is not passed through OpenCode argv. Existing sandbox and permission configuration remain unchanged.

## Tests

Add focused tests covering:

- the complete argv with model and profile options;
- the complete minimal argv;
- exclusion of cwd from argv;
- task delimiting after `--`;
- exclusion of unsupported or prohibited flags as distinct argv elements.

Strict private-server, canonical-cwd, sandbox, and permission invariants remain unchanged.

Focused command:

`cargo test --bin temote-mcp --locked local_agent::tests::opencode_`

## Verification status

- Focused source tests: PASS in Linux and macOS CI at `a3591d7`, including all three new exact-argv tests. The installed-runtime canary is separate and remains NOT RUN.
- Exact helper and all three new tests: type-checked successfully with `rustc --edition=2024 --crate-name opencode_argv_contract --test --emit=metadata` using an unchanged source extraction. This does not execute the tests or compile the complete repository.
- Extracted-source SHA-256: `6272f9f36eaaa950b3d33bd41032842fe59364eb5db4a86bf9052a95b6070995`.
- Formatting: `rustfmt --edition 2024 --check src/local_agent.rs` PASS.
- Whitespace: `git diff --check` PASS.
- Clippy checks: PASS on Linux and macOS at `a3591d7`.
- Full Linux CI: PASS at `a3591d7`, run `35709691217`. The macOS job still had seven unrelated descriptor-pinned Git failures; no whole-macOS success is claimed.
- Live rebuilt-runtime canary: NOT RUN.
- Parent `cargo check --all-targets --locked`: failed before rustc execution with `EPERM`; this is an environment blocker, not a source compiler result.

## Review boundary

The implementation and tests were authored by a delegated coding agent. The coordinator reviewed and mechanically integrated the patch, applied standard formatting, and ran the checks above. Interrupted GitHub PR broker changes were not included in this packet. This issue stays in `doing` until the supported runtime canary and remaining checks are verified; a source commit is not an operational completion claim.

No deployment or restart was performed.

CI and reviewer evidence: `docs/evaluations/20260922-interrupted-opencode-recovery-review.md`.

## 2026-09-22 consolidation: done

Repository-local implementation verified on `main`; remaining live/canary evidence is tracked in `issues/open/20260908-live-acceptance-matrix.md` under "Repository-completion live residuals".
