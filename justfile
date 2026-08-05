set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set dotenv-load := false

default:
    @just --list

# Install local-mcp and the Linux sandbox helper from the locked dependency set.
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
    public_env_file="${LOCAL_MCP_ENV_FILE:-${HOME}/.config/local-mcp/public.env}"; \
    test -r "$public_env_file" || { echo "missing readable environment file: $public_env_file" >&2; exit 1; }; \
    set -a; source "$public_env_file"; set +a; \
    for variable_name in LOCAL_MCP_PUBLIC_URL LOCAL_MCP_ACCESS_TEAM_DOMAIN LOCAL_MCP_ACCESS_AUDIENCE LOCAL_MCP_ACCESS_ALLOWED_EMAILS LOCAL_MCP_TUNNEL_TOKEN; do \
        test -n "${!variable_name:-}" || { echo "missing $variable_name in $public_env_file" >&2; exit 1; }; \
    done; \
    echo "public environment is configured: $public_env_file"

# Diagnose local-mcp and the host sandbox prerequisites.
doctor:
    local-mcp doctor

# Run the local HTTP origin. Keep this terminal running while ChatGPT is connected.
serve: env-check
    public_env_file="${LOCAL_MCP_ENV_FILE:-${HOME}/.config/local-mcp/public.env}"; \
    set -a; source "$public_env_file"; set +a; \
    exec local-mcp serve

# Run the on-demand remotely managed Cloudflare Tunnel.
tunnel: env-check
    public_env_file="${LOCAL_MCP_ENV_FILE:-${HOME}/.config/local-mcp/public.env}"; \
    set -a; source "$public_env_file"; set +a; \
    exec cloudflared tunnel run --token "$LOCAL_MCP_TUNNEL_TOKEN"

# Run the origin and Tunnel together. Ctrl-C stops both child processes.
up: env-check
    set +e; \
    public_env_file="${LOCAL_MCP_ENV_FILE:-${HOME}/.config/local-mcp/public.env}"; \
    set -a; source "$public_env_file"; set +a; \
    runtime_directory="${LOCAL_MCP_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-${HOME}/.cache}/local-mcp}"; \
    mkdir -p "$runtime_directory"; \
    pid_file="$runtime_directory/up.pids"; \
    if [ -s "$pid_file" ]; then \
        read -r existing_serve existing_tunnel < "$pid_file" || true; \
        if { [ -n "$existing_serve" ] && kill -0 "$existing_serve" 2>/dev/null; } || { [ -n "$existing_tunnel" ] && kill -0 "$existing_tunnel" 2>/dev/null; }; then \
            echo "local-mcp is already running; use just down first" >&2; \
            exit 1; \
        fi; \
        rm -f "$pid_file"; \
    fi; \
    local-mcp serve & serve_pid=$!; \
    cloudflared tunnel run --token "$LOCAL_MCP_TUNNEL_TOKEN" & tunnel_pid=$!; \
    printf '%s %s\n' "$serve_pid" "$tunnel_pid" > "$pid_file"; \
    cleanup() { kill "$serve_pid" "$tunnel_pid" 2>/dev/null || true; wait "$serve_pid" "$tunnel_pid" 2>/dev/null || true; rm -f "$pid_file"; }; \
    trap cleanup EXIT; \
    trap 'exit 130' INT; \
    trap 'exit 143' TERM; \
    while kill -0 "$serve_pid" 2>/dev/null && kill -0 "$tunnel_pid" 2>/dev/null; do sleep 1; done; \
    status=0; \
    if ! kill -0 "$serve_pid" 2>/dev/null; then wait "$serve_pid" || status=$?; fi; \
    if ! kill -0 "$tunnel_pid" 2>/dev/null; then wait "$tunnel_pid" || status=$?; fi; \
    exit "$status"

# Stop the origin and Tunnel started by `just up`.
down:
    runtime_directory="${LOCAL_MCP_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-${HOME}/.cache}/local-mcp}"; \
    pid_file="$runtime_directory/up.pids"; \
    if [ ! -r "$pid_file" ]; then \
        echo "no just up process is recorded"; \
        exit 0; \
    fi; \
    read -r serve_pid tunnel_pid < "$pid_file" || { echo "invalid PID file: $pid_file" >&2; exit 1; }; \
    stop_process() { \
        pid="$1"; expected="$2"; signal="$3"; \
        [ -n "$pid" ] || return 0; \
        actual=$(ps -o comm= -p "$pid" 2>/dev/null | tr -d ' '); \
        [ "$actual" = "$expected" ] || return 0; \
        kill "$signal" "$pid" 2>/dev/null || true; \
    }; \
    stop_process "$serve_pid" local-mcp -TERM; \
    stop_process "$tunnel_pid" cloudflared -TERM; \
    for _ in 1 2 3 4 5 6 7 8 9 10; do \
        alive=0; \
        for pid in "$serve_pid" "$tunnel_pid"; do \
            if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then alive=1; fi; \
        done; \
        [ "$alive" -eq 0 ] && break; \
        sleep 0.2; \
    done; \
    if [ "$alive" -ne 0 ]; then \
        stop_process "$serve_pid" local-mcp -KILL; \
        stop_process "$tunnel_pid" cloudflared -KILL; \
    fi; \
    rm -f "$pid_file"

# Start one permission-scoped local session. The optional directory controls its working directory.
start session_id working_directory=".":
    cd {{ quote(working_directory) }} && local-mcp start {{ quote(session_id) }}

# Run the local stdio MCP server for clients that launch it directly.
mcp:
    local-mcp mcp
