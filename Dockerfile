# Stage 1: Build the workspace binaries
FROM rust:1.80-slim-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/chotu

# Create a blank project structure to cache dependency builds
COPY Cargo.toml Cargo.lock ./
COPY chotu-common/Cargo.toml ./chotu-common/
COPY coordinator/Cargo.toml ./coordinator/
COPY health-coach/Cargo.toml ./health-coach/
COPY janitor/Cargo.toml ./janitor/
COPY streamer/Cargo.toml ./streamer/
COPY chotu-evals/Cargo.toml ./chotu-evals/

# Create dummy source files for all crates
RUN mkdir -p chotu-common/src coordinator/src health-coach/src janitor/src streamer/src chotu-evals/src && \
    echo "pub fn lib() {}" > chotu-common/src/lib.rs && \
    echo "fn main() {}" > coordinator/src/main.rs && \
    echo "pub fn run() {}" > health-coach/src/lib.rs && \
    echo "pub fn run() {}" > janitor/src/lib.rs && \
    echo "pub fn run() {}" > streamer/src/lib.rs && \
    echo "fn main() {}" > chotu-evals/src/main.rs

# Build dependencies only (this layer will be cached)
RUN cargo build --release

# Remove dummy sources
RUN rm -rf chotu-common/src coordinator/src health-coach/src janitor/src streamer/src chotu-evals/src

# Copy real sources
COPY chotu-common ./chotu-common
COPY coordinator ./coordinator
COPY health-coach ./health-coach
COPY janitor ./janitor
COPY streamer ./streamer
COPY chotu-evals ./chotu-evals

# Rebuild with real source code
RUN touch chotu-common/src/lib.rs coordinator/src/main.rs health-coach/src/lib.rs janitor/src/lib.rs streamer/src/lib.rs chotu-evals/src/main.rs && \
    cargo build --release --bin coordinator

# Stage 2: Runtime image
FROM debian:bookworm-slim

# Install runtime dependencies (OpenSSL and CA certificates for secure TLS requests)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    sqlite3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the compiled binary from the builder stage
COPY --from=builder /usr/src/chotu/target/release/coordinator /app/coordinator

# Copy default config file
COPY config.yaml.example /app/config.yaml

# Expose the port for the local OAuth redirect server
EXPOSE 8080

# Run the supervisor coordinator
ENTRYPOINT ["/app/coordinator"]
