# Project Chotu Makefile

# Load environment variables from .env file if it exists
ifneq (,$(wildcard ./.env))
    include .env
    export
endif

# Toolchain path helper
RUST_PATH := /Users/user/.rustup/toolchains/stable-aarch64-apple-darwin/bin

.PHONY: help setup prereqs run test clean

help:
	@echo "Available commands:"
	@echo "  make setup     - Create config.yaml and template .env file if they do not exist"
	@echo "  make prereqs   - Pull required local Ollama models (llama3.2:3b and deepseek-r1:8b)"
	@echo "  make run       - Run the supervisor coordinator"
	@echo "  make test      - Run unit tests across all crates"
	@echo "  make clean     - Clean cargo build artifacts"

setup:
	@if [ ! -f config.yaml ]; then \
		echo "Creating config.yaml from config.yaml.example..."; \
		cp config.yaml.example config.yaml; \
	else \
		echo "config.yaml already exists."; \
	fi
	@if [ ! -f .env ]; then \
		echo "Creating template .env file..."; \
		echo "# Project Chotu Environment Secrets" > .env; \
		echo "TELEGRAM_BOT_TOKEN=" >> .env; \
		echo "GEMINI_API_KEY=" >> .env; \
		echo "TELEGRAM_CHAT_ID=" >> .env; \
		echo "" >> .env; \
		echo "# Ollama Configuration" >> .env; \
		echo "OLLAMA_HOST=http://localhost" >> .env; \
		echo "OLLAMA_PORT=11434" >> .env; \
		echo "OLLAMA_MODEL=llama3.2:3b" >> .env; \
		echo "" >> .env; \
		echo "# App Configuration" >> .env; \
		echo "CHOTU_CONFIG_PATH=config.yaml" >> .env; \
		echo "CHOTU_BRAIN_DIR=~/chotu_brain" >> .env; \
		echo "DATABASE_PATH=chotu.db" >> .env; \
		echo "Please fill in your API keys in the .env file."; \
	else \
		echo ".env file already exists."; \
	fi

prereqs:
	@echo "Pulling Ollama models..."
	ollama pull llama3.2:3b
	ollama pull deepseek-r1:8b
	ollama pull qwen3.5:4b

run: setup
	@if [ -z "$$TELEGRAM_BOT_TOKEN" ] || [ -z "$$GEMINI_API_KEY" ]; then \
		echo "WARNING: TELEGRAM_BOT_TOKEN or GEMINI_API_KEY is not configured in your environment or .env file."; \
		echo "Please edit the .env file and add your credentials first."; \
		exit 1; \
	fi
	PATH="$(RUST_PATH):$$PATH" cargo run -p coordinator

test:
	PATH="$(RUST_PATH):$$PATH" cargo test --workspace

clean:
	PATH="$(RUST_PATH):$$PATH" cargo clean
