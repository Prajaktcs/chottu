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

# Pull required local Ollama models
prereqs:
    @echo "Pulling Ollama models..."
    ollama pull llama3.2:3b
    ollama pull deepseek-r1:8b
    ollama pull qwen3.5:4b

# Run the supervisor coordinator
run: setup
    #!/usr/bin/env bash
    if [ -z "$SIGNAL_CLI_SOCKET" ] || [ -z "$GEMINI_API_KEY" ]; then
        echo "WARNING: SIGNAL_CLI_SOCKET or GEMINI_API_KEY is not configured in your environment or .env file."
        echo "Please edit the .env file and add your credentials first."
        exit 1
    fi
    if [ ! -S "$SIGNAL_CLI_SOCKET" ]; then
        echo "SIGNAL_CLI_SOCKET is not a Unix socket: $SIGNAL_CLI_SOCKET"
        echo "Start signal-cli before just run."
        exit 1
    fi
    cargo run -p coordinator

# Build the full workspace
build:
    cargo build --workspace

# Format check + clippy (warnings allowed until -D is clean). Used by pre-commit.
lint:
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
