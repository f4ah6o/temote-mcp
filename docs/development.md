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

On Linux, install the sibling `codex-linux-sandbox` binary and make sure `bwrap` is on `PATH`. macOS uses the system Seatbelt sandbox. Native Windows is not supported.

Keep `--locked`: Linux sandbox dependencies are pinned to a Codex Git revision and the committed lockfile prevents incompatible prerelease transitive versions from being selected.

## Diagnostics

```sh
temote-mcp doctor
```

On Linux, `doctor` checks the installed sandbox helper, `bubblewrap`, user namespaces, the isolated network namespace, a real Temote MCP sandbox command, and the shell runtime environment. Required failures produce a non-zero exit status.

## Checks

```sh
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features --all-targets
(cd gateway && npm test)
git diff --check
```

The `justfile` provides `just build`, `just install`, `just doctor`, `just check`, and deployment-oriented recipes.

## Release versioning

Releases use CalVer `YYYY.MM.PATCH` in the `Asia/Tokyo` timezone through [`f4ah6o/calver-action`](https://github.com/f4ah6o/calver-action).

Move the `latest` tag to the desired commit in `main` history to request a release:

```sh
git tag -f latest <commit-to-release>
git push -f origin latest
```

`.github/workflows/release.yaml` allocates the next prefixless CalVer tag, updates `Cargo.toml` and `Cargo.lock` in a release-only commit, validates normal and local-only builds, and pushes the immutable CalVer tag. The release-only version commit is not merged back into `main`.
