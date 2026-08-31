set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
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

fmt-check:
    cargo fmt --all -- --check

test:
    cargo test

clippy:
    cargo clippy --all-targets -- -D warnings

gateway-test:
    npm test --prefix gateway

diff-check:
    git diff --check

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
