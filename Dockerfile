FROM rust:1.93-bookworm AS builder

WORKDIR /app

# Cache dependency build: copy manifests first, build with dummy src.
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto/ proto/
RUN mkdir -p src benches && \
    echo 'fn main() {}' > src/main.rs && \
    echo 'fn main() {}' > src/client.rs && \
    touch src/lib.rs && \
    echo 'fn main() {}' > benches/hot_path.rs && \
    cargo build --release && \
    rm -rf src benches

# Build the real binary.
COPY src/ src/
COPY benches/ benches/
RUN touch src/main.rs src/client.rs src/lib.rs benches/hot_path.rs && cargo build --release

# --- Runtime ---
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN useradd -r -s /bin/false app

COPY --from=builder /app/target/release/orderbook-aggregator /usr/local/bin/
COPY --from=builder /app/target/release/client /usr/local/bin/orderbook-client

USER app

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -f http://localhost:9090/health || exit 1

ENTRYPOINT ["orderbook-aggregator"]
