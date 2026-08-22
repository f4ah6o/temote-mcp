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

On Linux, `doctor` checks the installed sandbox helper, `bubblewrap`, user namespaces, the isolated network namespace, a real Temote MCP sandbox command, and the shell runtime environment. Required failures produce a non-zero exit status. When a Tunnel token file is configured or present at the default path, it also checks `cloudflared`, token-file readability, and Unix token-file permissions without printing the token.

To query the Cloudflare control plane, run:

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

The installed-binary lifecycle commands are `temote-mcp up` and `temote-mcp down`. The `justfile` provides development-oriented `just build`, `just install`, `just doctor`, `just check`, and wrappers that delegate `just up/down` to the checkout binary.

## Release versioning

Releases use CalVer `YYYY.MM.PATCH` in the `Asia/Tokyo` timezone through [`f4ah6o/calver-action`](https://github.com/f4ah6o/calver-action).

Move the `latest` tag to the desired commit in `main` history to request a release:

```sh
git tag -f latest <commit-to-release>
git push -f origin latest
```

`.github/workflows/release.yaml` allocates the next prefixless CalVer tag, updates `Cargo.toml` and `Cargo.lock` in a release-only commit, validates normal and local-only builds, and pushes the immutable CalVer tag. It then dispatches the generated cargo-dist workflow at that immutable tag. The release-only version commit is not merged back into `main`.

`dist-workspace.toml` is the source of truth for binary distribution. `dist generate` refreshes `.github/workflows/release.yml`; do not hand-edit the generated workflow. Releases currently build `.tar.xz` archives for Apple Silicon macOS plus ARM64 and x64 GNU/Linux, then publish them to GitHub Releases. Intel macOS is not supported.

`cargo-binstall` can install the published registry package with `cargo binstall temote-mcp`; release archives remain available through the repository's GitHub Release metadata. The package contains both `temote-mcp` and its Linux sibling helper, so a crates.io install remains self-contained.
