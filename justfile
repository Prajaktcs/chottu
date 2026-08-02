set dotenv-load := true

# Display available commands
default:
    @just --list

# Create config.yaml and template .env file if they do not exist
setup:
    #!/usr/bin/env bash
    if [ ! -f config.yaml ]; then
        echo "Creating config.yaml from config.yaml.example..."
        cp config.yaml.example config.yaml
    else
        echo "config.yaml already exists."
    fi
    if [ ! -f .env ]; then
        echo "Creating template .env file..."
        echo "# Project Chotu Environment Secrets" > .env
        echo "TELEGRAM_BOT_TOKEN=" >> .env
        echo "GEMINI_API_KEY=" >> .env
        echo "TELEGRAM_CHAT_ID=" >> .env
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
    if [ -z "$TELEGRAM_BOT_TOKEN" ] || [ -z "$GEMINI_API_KEY" ]; then
        echo "WARNING: TELEGRAM_BOT_TOKEN or GEMINI_API_KEY is not configured in your environment or .env file."
        echo "Please edit the .env file and add your credentials first."
        exit 1
    fi
    cargo run -p coordinator

# Build the full workspace
build:
    cargo build --workspace

# Run unit tests across all crates
test:
    cargo test --workspace

# Clean cargo build artifacts
clean:
    cargo clean
