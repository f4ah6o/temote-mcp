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
(cd gateway && npm test)
git diff --check
```

The installed-binary lifecycle commands are `temote-mcp up --profile cloudflare|tailscale|openai` and `temote-mcp down`. Omitting the profile remains equivalent to `cloudflare`. The `justfile` provides development-oriented Cloudflare wrappers through `just up/down`; Tailscale/OpenAI profile testing should invoke the checkout binary directly so Cloudflare-only environment checks are not applied. For OpenAI, `TUNNEL_CLIENT_BIN` can point at a checkout/test binary while production should use the supported `tunnel-client` distribution and a Restricted runtime key rather than an admin key.

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
