set shell := ["bash", "-e", "-o", "pipefail", "-c", "set -u; eval \"$0\""]
set dotenv-load := false

default:
    @just --list

# Build the repository version used by the development recipes below.
build:
    cargo build --release --locked

# Install temote-mcp and the Linux sandbox helper from the locked dependency set.
install:
    cargo install --path . --locked

# Format the Rust sources in place.
fmt:
    cargo fmt --all

# Run the checks used before publishing changes.
check: fmt-check test clippy gateway-test diff-check

# Run the deterministic subset that is valid from inside an already-sandboxed
# normal Temote developer session. This is intentionally not a replacement for
# `just check` or CI: tests that need Unix-socket syscalls, a nested bubblewrap
# namespace, nested process spawning, or a macOS Seatbelt host remain host/CI
# gates and are reported as NOT RUN here.
sandboxed-check: fmt-check sandboxed-test clippy sandboxed-no-default gateway-sandbox-test diff-check

sandboxed-test:
    cargo test --lib --all-features --locked -- --skip sandbox::linux_tests
    cargo test --bin temote-mcp --all-features --locked activity_job
    cargo test --bin temote-mcp --all-features --locked activity_coverage
    cargo test --bin temote-mcp --all-features --locked upgrade_transaction::tests::
    cargo test --bin temote-mcp --all-features --locked upgrade_coordinator::tests::
    cargo test --bin temote-mcp --all-features --locked session_control::tests::session_gc
    @echo "NOT RUN (host/CI gate): session_gc socket liveness acceptance (session_control::tests::host_liveness_tests)"
    @echo "NOT RUN (host/CI gate): Linux nested sandbox runtime tests (sandbox::linux_tests)"
    @echo "NOT RUN (host/CI gate): pinned-workspace path-swap bubblewrap acceptance (sandbox::linux::helper::tests::pinned_workspace_descriptor_survives_a_path_swap_host_acceptance)"
    @echo "NOT RUN (host/CI gate): agent-mode linked-worktree broker metadata acceptance (nested Linux sandbox required)"
    @echo "NOT RUN (host/CI gate): deployment-preflight CLI subprocess tests (nested process spawn required)"
    @echo "NOT RUN (host/CI gate): full binary/local Unix-socket integration suite"
    @echo "NOT RUN (host/CI gate): ignored supervisor/process-boundary E2E"
    @echo "NOT RUN (macOS/CI gate): macOS native Seatbelt tests"

sandboxed-no-default:
    cargo check --no-default-features --all-targets --locked

# The complete gateway suite, including the deployment-preflight CLI subprocess
# tests. Host/CI only: nested `node` process spawning can fail with EPERM inside
# a Temote sandbox.
gateway-test:
    npm test --prefix gateway

# Gateway evaluator tests that do not spawn nested processes. Safe for
# `just sandboxed-check`.
gateway-sandbox-test:
    npm run test:sandbox --prefix gateway

# Host-level Linux acceptance. This requires a production-shaped bubblewrap /
# AppArmor/userns environment and is expected to fail when invoked from inside
# an existing normal Temote sandbox.
linux-sandbox-acceptance:
    cargo build --bin temote-linux-sandbox --locked
    cargo test --lib --all-features --locked linux_tests -- --nocapture
    cargo test --lib --all-features --locked sandbox::linux::helper::tests::pinned_workspace_descriptor_survives_a_path_swap_host_acceptance -- --exact --ignored --nocapture

fmt-check:
    cargo fmt --all -- --check

test:
    cargo test

clippy:
    cargo clippy --all-targets -- -D warnings

diff-check:
    git diff --check

# List the mutants cargo-mutants would generate, without running tests.
# Scope with -f/--file or -F/-E regexes, e.g. `just mutants-list -f src/mcp.rs`.
mutants-list *args:
    cargo mutants --list {{ args }}

# Run mutation testing. Always scope it: `just mutants -f src/mcp.rs`,
# `just mutants -F 'line_protocol'`, or add `-- --skip <test-filter>` when the
# local baseline has known-failing tests. A full-tree run is a long host/CI job.
mutants *args:
    cargo mutants {{ args }}

# Mutation-test only the lines changed relative to a base ref (PR gate).
mutants-diff base="origin/main":
    git diff {{ quote(base) }}... > target/temote-pr.diff
    cargo mutants --in-diff target/temote-pr.diff

# Verify the private public.env file without printing any secret values.
env-check:
    public_env_file="${TEMOTE_MCP_ENV_FILE:-${HOME}/.config/temote-mcp/public.env}"; \
    test -r "$public_env_file" || { echo "missing readable environment file: $public_env_file" >&2; exit 1; }; \
    set -a; source "$public_env_file"; set +a; \
    for variable_name in TEMOTE_MCP_PUBLIC_URL TEMOTE_MCP_ACCESS_TEAM_DOMAIN TEMOTE_MCP_ACCESS_AUDIENCE TEMOTE_MCP_ACCESS_ALLOWED_EMAILS; do \
        test -n "${!variable_name:-}" || { echo "missing $variable_name in $public_env_file" >&2; exit 1; }; \
    done; \
    tunnel_token_file="${TUNNEL_TOKEN_FILE:-${HOME}/.config/temote-mcp/tunnel-token}"; \
    test -s "$tunnel_token_file" || { echo "missing or empty tunnel token file: $tunnel_token_file" >&2; exit 1; }; \
    echo "public environment is configured: $public_env_file"; \
    echo "tunnel token file is configured: $tunnel_token_file"

# Diagnose temote-mcp and the host sandbox prerequisites.
doctor: build
    "{{ justfile_directory() }}/target/release/temote-mcp" doctor

# Run the local HTTP origin. Keep this terminal running while ChatGPT is connected.
serve: build env-check
    public_env_file="${TEMOTE_MCP_ENV_FILE:-${HOME}/.config/temote-mcp/public.env}"; \
    set -a; source "$public_env_file"; set +a; \
    exec "{{ justfile_directory() }}/target/release/temote-mcp" serve

# Run the on-demand remotely managed Cloudflare Tunnel.
tunnel: env-check
    public_env_file="${TEMOTE_MCP_ENV_FILE:-${HOME}/.config/temote-mcp/public.env}"; \
    set -a; source "$public_env_file"; set +a; \
    tunnel_token_file="${TUNNEL_TOKEN_FILE:-${HOME}/.config/temote-mcp/tunnel-token}"; \
    exec cloudflared tunnel run --token-file "$tunnel_token_file"

# Run the origin and Tunnel together. The binary bootstraps compatible legacy config when needed.
up: build
    exec "{{ justfile_directory() }}/target/release/temote-mcp" up

# Stop the foreground supervisor started by temote-mcp up.
down:
    if command -v temote-mcp >/dev/null 2>&1; then \
        exec temote-mcp down; \
    else \
        exec "{{ justfile_directory() }}/target/release/temote-mcp" down; \
    fi

# Start one permission-scoped local session. The optional directory controls its working directory.
start session_id working_directory=".": build
    cd {{ quote(working_directory) }} && "{{ justfile_directory() }}/target/release/temote-mcp" start {{ quote(session_id) }}

# Run the local stdio MCP server for clients that launch it directly.
mcp: build
    "{{ justfile_directory() }}/target/release/temote-mcp" mcp
