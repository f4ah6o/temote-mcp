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
check: fmt-check test clippy diff-check

fmt-check:
    cargo fmt --all -- --check

test:
    cargo test

clippy:
    cargo clippy --all-targets -- -D warnings

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

# Run the origin and Tunnel together. temote-mcp stays in the foreground so it owns approval-console stdin.
up: build env-check
    public_env_file="${TEMOTE_MCP_ENV_FILE:-${HOME}/.config/temote-mcp/public.env}"; \
    set -a; source "$public_env_file"; set +a; \
    runtime_directory="${TEMOTE_MCP_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-${HOME}/.cache}/temote-mcp}"; \
    mkdir -p "$runtime_directory"; \
    pid_file="$runtime_directory/up.pid"; \
    if [ -s "$pid_file" ]; then \
        existing_serve=$(cat "$pid_file" 2>/dev/null || true); \
        if [ -n "$existing_serve" ] && kill -0 "$existing_serve" 2>/dev/null; then \
            echo "temote-mcp is already running; use just down first" >&2; \
            exit 1; \
        fi; \
        rm -f "$pid_file"; \
    fi; \
    tunnel_token_file="${TUNNEL_TOKEN_FILE:-${HOME}/.config/temote-mcp/tunnel-token}"; \
    printf '%s\n' "$$" > "$pid_file"; \
    export TEMOTE_MCP_UP_PID_FILE="$pid_file"; \
    exec "{{ justfile_directory() }}/target/release/temote-mcp" serve --tunnel-token-file "$tunnel_token_file"

# Stop the foreground supervisor started by `just up`; it gracefully stops its tunnel and managed sessions.
down:
    runtime_directory="${TEMOTE_MCP_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-${HOME}/.cache}/temote-mcp}"; \
    pid_file="$runtime_directory/up.pid"; \
    if [ ! -r "$pid_file" ]; then \
        echo "no just up process is recorded"; \
        exit 0; \
    fi; \
    serve_pid=$(cat "$pid_file" 2>/dev/null || true); \
    if [ -z "$serve_pid" ]; then echo "invalid PID file: $pid_file" >&2; rm -f "$pid_file"; exit 1; fi; \
    actual=$(ps -o comm= -p "$serve_pid" 2>/dev/null | tr -d ' '); \
    if [ "$actual" != "temote-mcp" ]; then rm -f "$pid_file"; echo "recorded temote-mcp process is not running"; exit 0; fi; \
    tunnel_pids=$(pgrep -P "$serve_pid" -x cloudflared 2>/dev/null || true); \
    kill -TERM "$serve_pid" 2>/dev/null || true; \
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do \
        kill -0 "$serve_pid" 2>/dev/null || break; \
        sleep 0.2; \
    done; \
    if kill -0 "$serve_pid" 2>/dev/null; then \
        for child in $tunnel_pids; do kill -KILL "$child" 2>/dev/null || true; done; \
        kill -KILL "$serve_pid" 2>/dev/null || true; \
    fi; \
    rm -f "$pid_file"

# Start one permission-scoped local session. The optional directory controls its working directory.
start session_id working_directory=".": build
    cd {{ quote(working_directory) }} && "{{ justfile_directory() }}/target/release/temote-mcp" start {{ quote(session_id) }}

# Run the local stdio MCP server for clients that launch it directly.
mcp: build
    "{{ justfile_directory() }}/target/release/temote-mcp" mcp
