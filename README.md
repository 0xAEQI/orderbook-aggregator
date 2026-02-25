# Order Book Aggregator

Real-time order book aggregator that connects to **Binance** and **Bitstamp** WebSocket feeds, merges their order books, and streams the top-10 bid/ask levels with spread via **gRPC**.

## Architecture

```
WebSocket Feeds            Merger              gRPC Server
┌──────────┐              ┌───────┐           ┌──────────┐
│ Binance  │─broadcast──▶│       │──watch──▶ │          │──stream──▶ Client
│ depth20  │              │ Merge │           │  tonic   │──stream──▶ Client
│          │              │ Top10 │           │          │
│ Bitstamp │─broadcast──▶│       │           │          │
│ order_book│             └───────┘           └──────────┘
└──────────┘
```

- **Exchange adapters** connect via WebSocket with automatic reconnection (exponential backoff + jitter)
- **Merger** maintains latest book per exchange, merges on every update, publishes top-10 via `tokio::watch`
- **gRPC server** streams merged summaries to all connected clients

## Build & Run

```bash
# Build
cargo build --release

# Run with default settings (ethbtc on port 50051)
cargo run --release

# Custom pair and port
cargo run --release -- --symbol btcusdt --port 50052

# Enable debug logging
RUST_LOG=debug cargo run --release
```

## Configuration

| Flag | Default | Description |
|------|---------|-------------|
| `-s, --symbol` | `ethbtc` | Trading pair symbol |
| `-p, --port` | `50051` | gRPC server port |

## Testing the gRPC Stream

```bash
# Using grpcurl
grpcurl -plaintext -import-path proto -proto orderbook.proto \
  localhost:50051 orderbook.OrderbookAggregator/BookSummary
```

## Proto Schema

The gRPC service is defined in `proto/orderbook.proto`:

```protobuf
service OrderbookAggregator {
  rpc BookSummary(Empty) returns (stream Summary);
}

message Summary {
  double spread = 1;
  repeated Level bids = 2;  // Top 10, highest price first
  repeated Level asks = 3;  // Top 10, lowest price first
}

message Level {
  string exchange = 1;
  double price = 2;
  double amount = 3;
}
```

## Design Decisions

- **`tokio::broadcast`** for exchange → merger: supports multiple receivers, handles backpressure via lagging
- **`tokio::watch`** for merger → gRPC: latest-value semantics — clients always get the most recent merged book
- **Partial depth streams**: Binance `depth20@100ms` provides snapshots directly (no sequence tracking needed)
- **Exponential backoff with jitter**: prevents thundering herd on reconnection
- **`CancellationToken`**: cooperative shutdown across all async tasks
