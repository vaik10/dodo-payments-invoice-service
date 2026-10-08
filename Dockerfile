# ==============================================================================
# Stage 1: Builder
# ==============================================================================
FROM rust:1.89-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies required for compiling Rust crates
RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace and member Cargo manifest files
COPY Cargo.toml Cargo.lock ./
COPY invoice-service/Cargo.toml ./invoice-service/Cargo.toml
COPY mock-psp/Cargo.toml ./mock-psp/Cargo.toml

# Create dummy source files to cache dependency compilation
RUN mkdir -p invoice-service/src mock-psp/src \
    && echo "fn main() {}" > invoice-service/src/main.rs \
    && echo "fn main() {}" > mock-psp/src/main.rs

# Build dependencies in release mode
RUN cargo build --release

# Clean up dummy sources and crate build artifacts so actual source rebuilds cleanly
RUN rm -rf invoice-service/src mock-psp/src \
    && rm -f target/release/deps/invoice_service* \
    && rm -f target/release/deps/mock_psp* \
    && rm -f target/release/invoice-service* \
    && rm -f target/release/mock-psp*

# Copy actual crate source code (migrations are embedded at compile time by sqlx::migrate!)
COPY invoice-service ./invoice-service
COPY mock-psp ./mock-psp

# Build the release binaries for the workspace
RUN cargo build --release

# ==============================================================================
# Stage 2: invoice-service runtime
# ==============================================================================
FROM debian:bookworm-slim AS invoice-service

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy compiled binary
COPY --from=builder /app/target/release/invoice-service ./invoice-service

EXPOSE 8080

CMD ["./invoice-service"]

# ==============================================================================
# Stage 3: mock-psp runtime
# ==============================================================================
FROM debian:bookworm-slim AS mock-psp

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy compiled mock-psp binary
COPY --from=builder /app/target/release/mock-psp ./mock-psp

EXPOSE 9090

CMD ["./mock-psp"]
