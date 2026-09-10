# Development

Repository-specific instructions for coding agents are in [`AGENTS.md`](../AGENTS.md). This page is the human contributor reference.

## Build and install

```sh
cargo build --release --locked
cargo install --path . --locked
```

The default build includes public HTTP and `gateway-agent`. A local-only build without Temote MCP's direct HTTP/JWT dependencies is available with:

```sh
cargo build --release --no-default-features --locked
```

On Linux, build or install the sibling `temote-linux-sandbox` binary and make sure `bwrap` is on `PATH`. macOS uses the system Seatbelt sandbox. Native Windows is not supported. See [Linux sandbox and crates.io packaging](linux-sandbox.md) for the helper boundary and acceptance tests.

Keep `--locked`: the committed lockfile prevents incompatible transitive versions from being selected, and the published package is intentionally registry-only.

## Diagnostics

```sh
temote-mcp doctor
```

On Linux, `doctor` checks the installed sandbox helper, `bubblewrap`, user namespaces, the isolated network namespace, a real Temote MCP sandbox command, and the shell runtime environment. Required failures produce a non-zero exit status. Bare `doctor` preserves the legacy Cloudflare auto-detection behavior. Provider-specific deployment checks are explicit:

```sh
temote-mcp doctor --profile cloudflare
temote-mcp doctor --profile tailscale
temote-mcp doctor --profile openai
```

The Cloudflare profile checks `cloudflared`, token-file readability/private permissions, and Cloudflare Access configuration. The Tailscale profile checks the CLI/node, canonical `*.ts.net` identity, existing Funnel ownership on HTTPS ports `443`/`8443`/`10000`, the first port Temote can safely own, and the process-local OAuth state. Tailscale diagnostics do not require or load the Cloudflare `public.env`. The OpenAI profile checks the official `tunnel-client`, `CONTROL_PLANE_TUNNEL_ID`, runtime-key availability/control-plane access, and the loopback-only local origin policy without requiring Cloudflare or Tailscale.

To additionally query the Cloudflare control plane, run:

```sh
temote-mcp doctor --cloudflare
```

This uses the official Cloudflare Cloudflared Tunnel API. Set `TEMOTE_MCP_CLOUDFLARE_ACCOUNT_ID`, `TEMOTE_MCP_CLOUDFLARE_TUNNEL_ID`, and `TEMOTE_MCP_CLOUDFLARE_API_TOKEN`; the corresponding `CLOUDFLARE_*` names are also accepted. The API token is read from the environment and never printed. The check reports Cloudflare's `inactive`, `degraded`, `healthy`, or `down` Tunnel state.

## Checks

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
npm test --prefix gateway
git diff --check
```

`just check`, pull-request CI, and release validation all run the gateway suite. The checked-in `gateway/contract/routed-tools.json` snapshot is generated from Rust's public non-supervisor tool surface and compared with the Worker export. Regenerate an intentional contract change with `TEMOTE_MCP_UPDATE_GATEWAY_CONTRACT=1 cargo test routed_gateway_contract_matches_checked_in_snapshot`, then review the structural diff.

The installed HTTP/ingress lifecycle commands are `temote-mcp up --profile cloudflare|tailscale|openai` and `temote-mcp down`. They require a separately running `temote-mcp supervisor`; `down` does not stop that lifecycle supervisor or its sessions. Omitting the profile remains equivalent to `cloudflare`. The `justfile` provides development-oriented Cloudflare wrappers through `just up/down`; Tailscale/OpenAI profile testing should invoke the checkout binary directly so Cloudflare-only environment checks are not applied. For OpenAI, `TUNNEL_CLIENT_BIN` can point at a checkout/test binary while production should use the supported `tunnel-client` distribution and a Restricted runtime key rather than an admin key.

## Experimental Codex delegation and app-server

The developer-only `temote-mcp codex delegate` command is a small dogfood/experimental bootstrap for a parent process that needs to run the installed Codex CLI non-interactively:

```sh
temote-mcp codex delegate \
  --model <model> \
  --reasoning-effort <effort> \
  --prompt-file ./delegation-prompt.txt
```

It invokes the installed `codex exec` contract with an explicit model, `model_reasoning_effort` config override, JSONL events, an output schema, and an output-last-message file. Child stdin is `/dev/null`, and JSONL/stderr are retained as owner-only temporary artifacts outside the checkout by default. The parent-facing result contains only a bounded JSON report, process classification, requested model/effort, observed model/effort when an event actually supplies them, and bounded thread/usage evidence; it does not print raw JSONL or stderr. The default final-report limit is 4096 bytes.

This bootstrap is developer infrastructure, not a generic remote shell. It does not widen filesystem roots, bypass approvals, or alter the direct `execute` network prohibition.

The experimental MCP surfaces `codex_status`, `codex_task_start`, `codex_task_get`, and `codex_task_control` use the local `codex app-server --stdio` protocol. The initialize handshake must advertise the supported app-server version (`0.153.4`) and a Codex home. Task records are bound to the complete Temote session instance (`session_id`, start time, and process ID) plus the canonical session directory. Every mutation requires an opaque `operation_id`; its accepted receipt is persisted before spawning or sending a child request. Pre-thread initialization/model-list failures are retryable with the same start operation, while a failure after thread/start or turn/start may have been sent becomes `reconciliation_required` and is never replayed blindly. Control is limited to typed `steer`, `resume`, and `interrupt` actions, and accepted operation IDs remain conflict-protected through task retention even when detailed receipts are compacted.

Normal-session task start/control requests require local approval. Temote yolo does not change the Codex child approval policy: app-server command/file-change requests still require the explicit user-approval path and decline when that transport is unavailable. The integration exposes named status/task methods only; it does not expose arbitrary JSON-RPC, `thread/shellCommand`, remote shell, or automatic approval. Task prompts and control input are not written to task metadata or approval/activity summaries. `codex_task_get` reconciles the remote thread before returning `not_modified`; unexpired task records, including terminal records, are not pruned for capacity. Only expired terminal records without a live runtime are eligible for pruning, and a full retention limit rejects new starts. Thread data is retained only as bounded, expiring, session-and-scope-bound in-memory evidence read through an opaque reference.

Generated Codex turns are requested with `workspaceWrite`, the canonical session directory as the writable root, and `networkAccess=false`. This is an app-server sandbox adapter, not a replacement for Temote's OS-level sandbox around direct commands: the app-server process itself must communicate with the inference service outside that direct command sandbox. Keep these MCP surfaces opt-in/experimental until the host's installed Codex build and sandbox behavior have been validated; do not treat them as equivalent to normal `execute`.

The structured `local_agent_run` broker is a separate adapter for one-shot Codex and OpenCode work. Its Rust side resolves only an absolute installed executable, constructs `codex exec --ignore-user-config --ephemeral --sandbox ... --cd ... --json` or `opencode run --pure --format json --dir ...`, and passes the task only after the CLI's option boundary; these contracts were checked with the installed `codex --help`, `codex exec --help`, `opencode --help`, and `opencode run --help` commands. It clears the child environment, supplies private per-run state, enforces canonical session-root scope and protected metadata rules, and uses the explicit user-approval path before every launch, including yolo sessions. This broker is intentionally limited to `local_agent_run`; it is not a Cargo or Vite+ broker, and it does not provide Git remote mutation.

For OpenAI bootstrap testing, `temote-mcp openai setup --workspace-id <id>` calls the production Tunnel Management API. When `OPENAI_ADMIN_KEY` is unset it uses a controlling-terminal hidden prompt; the returned tunnel ID alone is stored in `~/.config/temote-mcp/openai.env` (`0600`). Use `--config-file` for an isolated test path. The command refuses to overwrite an existing tunnel ID unless `--force` is explicit. `temote-mcp up --profile openai` similarly prompts for the Runtime API key when neither runtime-key environment variable is present, injects it only into the `tunnel-client` child, removes `OPENAI_ADMIN_KEY` from that child, and zeroizes the prompt buffer after spawn. Runtime/Admin keys are never persisted by these commands.

### Property-based tests

Security and path-containment invariants use [`noprop`](https://github.com/sile/noprop). The suite uses a deterministic default seed so failures reproduce under a normal `cargo test`. To replay or explore with another seed, set `TEMOTE_PBT_SEED` to a decimal or hexadecimal `u64`:

```sh
TEMOTE_PBT_SEED=0x1234 cargo test --all-features
```

Keep example tests for named regressions; use property tests for grammars, containment/fail-closed rules, redaction, and state-machine invariants where the input space is larger than a useful example table.

## Release versioning

Releases use CalVer `YYYY.MM.PATCH` in the `Asia/Tokyo` timezone through [`f4ah6o/calver-action`](https://github.com/f4ah6o/calver-action).

Move the `latest` tag to the desired commit in `main` history to request a release:

```sh
git tag -f latest <commit-to-release>
git push -f origin latest
```

`.github/workflows/release.yaml` allocates the next prefixless CalVer tag, updates `Cargo.toml` and `Cargo.lock` in a release-only commit, validates normal and local-only builds, and pushes the immutable CalVer tag. It then authenticates to crates.io through Trusted Publishing (GitHub OIDC, environment `release`) and runs `cargo publish --locked` without a long-lived crates.io secret in GitHub. Finally it dispatches the generated cargo-dist workflow at that immutable tag. The release-only version commit is not merged back into `main`.

`dist-workspace.toml` is the source of truth for binary distribution. `dist generate` refreshes `.github/workflows/release.yml`; do not hand-edit the generated workflow. Releases currently build `.tar.xz` archives for Apple Silicon macOS plus ARM64 and x64 GNU/Linux, then publish them to GitHub Releases. Intel macOS is not supported.

`cargo-binstall` can install the published registry package with `cargo binstall temote-mcp`; release archives remain available through the repository's GitHub Release metadata. The package contains both `temote-mcp` and its Linux sibling helper, so a crates.io install remains self-contained.
