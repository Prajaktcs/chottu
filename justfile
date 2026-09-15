set dotenv-load := true

# Display available commands
default:
    @just --list

# Create config.yaml and template .env file if they do not exist
setup:
    #!/usr/bin/env bash
    set -euo pipefail
    if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        if ! git config --local core.hooksPath .githooks; then
            echo "Failed to install git hooks." >&2
            exit 1
        fi
        echo "Git hooks: .githooks (pre-commit → just lint, pre-push → just test)"
    else
        echo "Skipping git hooks install (not inside a git worktree)."
    fi
    if [ ! -f config.yaml ]; then
        echo "Creating config.yaml from config.yaml.example..."
        cp config.yaml.example config.yaml
    else
        echo "config.yaml already exists."
    fi
    if [ ! -f .env ]; then
        echo "Creating template .env file..."
        echo "# Project Chotu Environment Secrets" > .env
        echo "SIGNAL_ACCOUNT=" >> .env
        echo "SIGNAL_CLI_DATA_DIR=" >> .env
        echo "SIGNAL_CLI_SOCKET=" >> .env
        echo "SIGNAL_GROUP_ID=" >> .env
        echo "GEMINI_API_KEY=" >> .env
        echo "" >> .env
        echo "# Ollama Configuration" >> .env
        echo "OLLAMA_HOST=http://localhost" >> .env
        echo "OLLAMA_PORT=11434" >> .env
        echo "OLLAMA_MODEL=qwen3.5:4b" >> .env
        echo "" >> .env
        echo "# App Configuration" >> .env
        echo "CHOTU_CONFIG_PATH=config.yaml" >> .env
        echo "CHOTU_BRAIN_DIR=~/chotu_brain" >> .env
        echo "DATABASE_PATH=chotu.db" >> .env
        echo "Please fill in your API keys in the .env file."
    else
        echo ".env file already exists."
    fi
    missing_runtime_helper=false
    for command in nc plutil; do
        if ! command -v "$command" >/dev/null 2>&1; then
            echo "Missing runtime prerequisite: $command (required by just run)." >&2
            missing_runtime_helper=true
        fi
    done
    if [ "$missing_runtime_helper" = true ]; then
        echo "nc and plutil ship with macOS; restore them with a macOS update or reinstall, then rerun just setup." >&2
        exit 1
    fi
    plutil_probe='{"jsonrpc":"2.0","id":1,"result":[]}'
    plutil_probe_jsonrpc="$(printf '%s\n' "$plutil_probe" \
        | plutil -extract jsonrpc xml1 -o - - 2>/dev/null || true)"
    plutil_probe_id="$(printf '%s\n' "$plutil_probe" \
        | plutil -extract id xml1 -o - - 2>/dev/null || true)"
    plutil_probe_result="$(printf '%s\n' "$plutil_probe" \
        | plutil -extract result xml1 -o - - 2>/dev/null || true)"
    plist_root=$'<plist version="1.0">\n'
    if [[ "$plutil_probe_jsonrpc" != *"$plist_root<string>2.0</string>"* ]] \
        || [[ "$plutil_probe_id" != *"$plist_root<integer>1</integer>"* ]] \
        || [[ "$plutil_probe_result" != *"$plist_root<array/>"* ]]; then
        echo "Installed plutil lacks the JSON support required by just run." >&2
        echo "Update macOS, then rerun just setup." >&2
        exit 1
    fi

# Pull required local Ollama models
prereqs:
    @echo "Pulling Ollama models..."
    ollama pull llama3.2:3b
    ollama pull deepseek-r1:8b
    ollama pull qwen3.5:4b

# Run signal-cli when needed, then start the supervisor coordinator
run: setup
    #!/usr/bin/env bash
    set -e

    if [ -z "$SIGNAL_CLI_SOCKET" ] || [ -z "$GEMINI_API_KEY" ]; then
        echo "WARNING: SIGNAL_CLI_SOCKET and GEMINI_API_KEY must be configured in your environment or .env file."
        echo "Please edit the .env file and add your credentials first."
        exit 1
    fi
    for command in nc plutil; do
        if ! command -v "$command" >/dev/null 2>&1; then
            echo "just run requires $command to probe SIGNAL_CLI_SOCKET." >&2
            echo "$command ships with macOS; restore it with a macOS update or reinstall, then retry." >&2
            exit 1
        fi
    done

    # Keep the Mac awake for the Signal websocket while Chotu runs.
    # Display may still sleep (-d intentionally omitted). No-op if caffeinate is absent.
    if [ -z "${CHOTU_CAFFEINATE:-}" ] && command -v caffeinate >/dev/null 2>&1; then
        export CHOTU_CAFFEINATE=1
        echo "Preventing idle/system sleep for this session (caffeinate -ims); display may still sleep."
        exec caffeinate -ims -- "$0"
    fi
    signal_cli_pid=""
    run_lock="${SIGNAL_CLI_SOCKET}.run.lock"
    lock_held=false
    lock_owner() {
        if [ -d "$run_lock" ] && [ ! -L "$run_lock" ]; then
            cat "$run_lock/pid" 2>/dev/null || true
        fi
    }
    cleanup() {
        if [ -n "$signal_cli_pid" ]; then
            kill "$signal_cli_pid" 2>/dev/null || true
            wait "$signal_cli_pid" 2>/dev/null || true
        fi
        if [ "$lock_held" = true ] && [ "$(lock_owner)" = "$$" ]; then
            rm -f "$run_lock/pid"
            rmdir "$run_lock" 2>/dev/null || true
        fi
    }
    socket_ready() {
        [ -S "$SIGNAL_CLI_SOCKET" ] || return 1
        local response jsonrpc_xml response_id_xml result_xml plist_root
        response="$(printf '%s\n' '{"jsonrpc":"2.0","method":"getUserStatus","params":{},"id":1}' \
            | nc -U -w 1 "$SIGNAL_CLI_SOCKET" 2>/dev/null || true)"
        [ -n "$response" ] || return 1
        jsonrpc_xml="$(printf '%s\n' "$response" \
            | plutil -extract jsonrpc xml1 -o - - 2>/dev/null || true)"
        response_id_xml="$(printf '%s\n' "$response" \
            | plutil -extract id xml1 -o - - 2>/dev/null || true)"
        result_xml="$(printf '%s\n' "$response" \
            | plutil -extract result xml1 -o - - 2>/dev/null || true)"
        plist_root=$'<plist version="1.0">\n'
        [[ "$jsonrpc_xml" == *"$plist_root<string>2.0</string>"* ]] \
            && [[ "$response_id_xml" == *"$plist_root<integer>1</integer>"* ]] \
            && { [[ "$result_xml" == *"$plist_root<array/>"* ]] \
                || [[ "$result_xml" == *"$plist_root<array>"* ]]; }
    }
    acquire_run_lock() {
        local lock_pid=""
        if mkdir "$run_lock" 2>/dev/null; then
            if ! printf '%s\n' "$$" > "$run_lock/pid"; then
                rm -f "$run_lock/pid"
                rmdir "$run_lock" 2>/dev/null || true
                echo "Cannot write run lock owner: $run_lock/pid" >&2
                exit 1
            fi
            lock_held=true
            return
        fi
        if [ -L "$run_lock" ] || { [ -e "$run_lock" ] && [ ! -d "$run_lock" ]; }; then
            echo "Cannot use run lock: $run_lock" >&2
            echo "Remove it if no just run process is active, then retry." >&2
            exit 1
        fi
        if [ ! -e "$run_lock" ]; then
            echo "Cannot create run lock: $run_lock" >&2
            echo "Ensure its parent directory exists and is writable, then retry." >&2
            exit 1
        fi
        for _ in 1 2 3 4; do
            lock_pid="$(lock_owner)"
            [ -n "$lock_pid" ] && break
            sleep 0.1
        done
        case "$lock_pid" in
            '')
                echo "Run lock has no owner PID: $run_lock" >&2
                echo "Remove it if no just run process is active, then retry." >&2
                exit 1
                ;;
            *[!0-9]*)
                echo "Cannot read the owner PID from run lock: $run_lock" >&2
                echo "Remove it if no just run process is active, then retry." >&2
                exit 1
                ;;
        esac
        if kill -0 "$lock_pid" 2>/dev/null; then
            echo "Another just run process is already using $SIGNAL_CLI_SOCKET"
            exit 1
        fi
        echo "Stale run lock: $run_lock (owner PID $lock_pid is not running)." >&2
        echo "Remove the lock directory, then retry." >&2
        exit 1
    }
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    acquire_run_lock
    if ! socket_ready; then
        if [ -e "$SIGNAL_CLI_SOCKET" ] && [ ! -S "$SIGNAL_CLI_SOCKET" ]; then
            echo "SIGNAL_CLI_SOCKET exists but is not a Unix socket: $SIGNAL_CLI_SOCKET"
            exit 1
        fi
        if [ -S "$SIGNAL_CLI_SOCKET" ]; then
            echo "Removing stale signal-cli socket: $SIGNAL_CLI_SOCKET"
            rm -f "$SIGNAL_CLI_SOCKET"
        fi
        if [ -z "$SIGNAL_CLI_DATA_DIR" ] || [ -z "$SIGNAL_ACCOUNT" ]; then
            echo "SIGNAL_CLI_DATA_DIR and SIGNAL_ACCOUNT are required to start signal-cli."
            exit 1
        fi
        if ! command -v signal-cli >/dev/null 2>&1; then
            echo "signal-cli is not installed. Install it with: brew install signal-cli"
            exit 1
        fi

        echo "Starting signal-cli daemon on $SIGNAL_CLI_SOCKET..."
        signal-cli --data-dir "$SIGNAL_CLI_DATA_DIR" --account "$SIGNAL_ACCOUNT" daemon \
            --receive-mode=manual --socket "$SIGNAL_CLI_SOCKET" &
        signal_cli_pid=$!

        for _ in {1..100}; do
            socket_ready && break
            if ! kill -0 "$signal_cli_pid" 2>/dev/null; then
                wait "$signal_cli_pid"
            fi
            sleep 0.1
        done

        if ! socket_ready; then
            echo "signal-cli did not become ready on: $SIGNAL_CLI_SOCKET"
            exit 1
        fi
    fi

    cargo run -p coordinator

# Build the full workspace
build:
    cargo build --workspace

# Lint Markdown with the same markdownlint-cli2 version used by CI.
markdownlint:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v npx >/dev/null 2>&1; then
        echo "markdownlint: npx is required (install Node.js)" >&2
        exit 1
    fi
    npx --yes markdownlint-cli2@0.23.2 "*.md" "docs/**/*.md" "evals/**/*.md" "!evals/research/results/**"

# Markdown, format, and clippy checks. Used by pre-commit.
lint: markdownlint
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked

# Run unit tests across all crates (matches CI). Used by pre-push.
test:
    cargo test --workspace --locked --all-targets

# Point this clone at .githooks/ (pre-commit lint, pre-push tests)
hooks:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        echo "just hooks: not inside a git worktree; cannot install hooks." >&2
        exit 1
    fi
    git config --local core.hooksPath .githooks
    echo "Installed git hooks from .githooks (pre-commit → just lint, pre-push → just test)"

# Clean cargo build artifacts
clean:
    cargo clean
