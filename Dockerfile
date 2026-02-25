FROM rust:1.93-bookworm AS builder

WORKDIR /app

# Install protobuf compiler.
RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

# Cache dependency build: copy manifests first, build with dummy src.
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto/ proto/
RUN mkdir -p src && \
    echo 'fn main() {}' > src/main.rs && \
    echo 'fn main() {}' > src/client.rs && \
    cargo build --release && \
    rm -rf src

# Build the real binary.
COPY src/ src/
RUN touch src/main.rs src/client.rs && cargo build --release

# --- Runtime ---
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/orderbook-aggregator /usr/local/bin/
COPY --from=builder /app/target/release/client /usr/local/bin/orderbook-client

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -f http://localhost:9090/health || exit 1

ENTRYPOINT ["orderbook-aggregator"]
