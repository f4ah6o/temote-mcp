# Linux sandbox and crates.io packaging

Temote owns its Linux sandbox boundary directly. The published `temote-mcp` Cargo package contains three binaries:

- `temote-mcp`
- `temote-linux-sandbox`
- `temote-onepassword-sdk`

`cargo install temote-mcp` installs all three binaries from the same package. There are no runtime or build dependencies on `codex-*` or `unofficial-codex-*` crates.

## Security model

The parent constructs a Temote-specific, versioned `LinuxSandboxPolicy`. It contains only the current working directory, explicit writable roots, temporary roots, read-only protected paths, and the fixed restricted-network mode. The helper parses this closed policy with unknown fields rejected and revalidates canonical paths before building the sandbox.

`temote-linux-sandbox` always enters bubblewrap before the requested command executes. The sandbox uses a read-only bind of `/`, explicit writable binds for the working directory and approved roots, `/tmp` and `TMPDIR` according to Temote's existing semantics, user/PID/network namespaces, `--die-with-parent`, and `--new-session`. Protected metadata such as `.git`, `.agents`, and `.codex` remains read-only for ordinary commands. Validated Git operations can write the repository metadata needed for add/commit while configuration, hooks, unrelated refs, and other protected metadata remain read-only.

The helper compiles Temote's network/process seccomp policy with `seccompiler`, writes the compiled cBPF program to a sealed anonymous memfd, and passes that fd to bubblewrap using `--seccomp FD`. There is no hidden self-dispatch stage that can execute a command outside bubblewrap. Bubblewrap establishes `no_new_privs` before applying the seccomp filter to the sandbox child.

Git metadata discovery rejects symbolic-link `.git` entries, malformed or multi-line pointer files, unexpected `commondir` use, and linked-worktree metadata that does not resolve to Git's expected `common/.git/worktrees/<name>` relationship and back-pointer.

## OpenAI Codex provenance

The Linux sandbox migration was based on an audit of OpenAI Codex at revision:

`20fedafff83f5c681fc62f73b0ca3227e42e3f8b`

Repository: `https://github.com/openai/codex`
License: Apache License 2.0
Attribution: OpenAI Codex, Copyright 2025 OpenAI

The design and implementation were informed by the Linux isolation behavior in these upstream areas:

- `codex-rs/sandboxing/src/landlock.rs`
- `codex-rs/linux-sandbox/src/linux_run_main.rs`
- `codex-rs/linux-sandbox/src/landlock.rs`
- `codex-rs/linux-sandbox/src/bwrap.rs`
- `codex-rs/linux-sandbox/src/launcher.rs`

Temote intentionally does **not** retain the Codex `PermissionProfile`, protocol model, configuration, MCP, login, model, UI, telemetry, cloud configuration, or network-proxy topology. The local implementation is concentrated in `src/sandbox/linux/` and uses a smaller Temote-specific policy and helper protocol.

Local modifications include Temote's Git metadata rules, linked-worktree validation, protected `.agents`/`.codex` handling, sibling-helper packaging, serialized-policy validation, sealed-memfd seccomp delivery, and Temote-specific regression/live acceptance tests.

The Apache-2.0 text is included as `LICENSE-APACHE`, and the corresponding attribution is recorded in `THIRD_PARTY_NOTICES.md` and `THIRD_PARTY_NOTICES.ja.md`.

## Acceptance

Linux CI installs bubblewrap and runs the live sandbox tests. The acceptance covers writable cwd and explicit roots, `/tmp`, denied writes outside permitted roots, ordinary Git metadata protection, real `git add`/`git commit`, linked worktrees, restricted networking, `no_new_privs`, malformed policy/helper misuse, and Git pointer rejection.

Packaging CI also runs `cargo package`, inspects the generated `.crate` for forbidden Git/Codex dependencies and required helper sources, installs from the extracted package into a temporary Cargo root, and verifies that all three binaries are present. Runtime dependency metadata is checked so `codex-*` and `unofficial-codex-*` cannot silently re-enter the published graph.
